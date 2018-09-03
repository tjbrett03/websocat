#![allow(unused)]
extern crate libloading;
extern crate futures;

use self::libloading::{Library, Symbol};

use ::std::ffi::CStr;
use ::std::os::raw::c_void;

use ::std::cell::{RefCell};

use ::std::io::{Read, Write};
use ::tokio_io::{AsyncRead, AsyncWrite};

use ::std::sync::{Arc};

use super::util::{wouldblock,simple_err,peer_err};

use super::{Result,Peer};

use self::futures::sync::mpsc::{channel,Sender,Receiver};
use self::futures::stream::{Stream};
use self::futures::future::{Future,ok};
use self::futures::sink::{Sink,Send as SinkSend};
use self::futures::{Poll,Async};

use ::std::rc::Rc;
use ::{Specifier,ConstructParams,PeerConstructor,BoxedNewPeerFuture,once};
use ::std::cell::UnsafeCell;


// bindgen --whitelist-var 'WEBSOCAT_.*' --whitelist-type 'websocat_.*'  plugin_api/websocat_plugin_ptr.h > src/plugin_api.rs

#[path = "plugin_api.rs"]
#[allow(non_camel_case_types)]
mod plugin_api;


#[derive(Debug, Clone)]
pub struct PluginConnect(pub String);
impl Specifier for PluginConnect {
    fn construct(&self, p: ConstructParams) -> PeerConstructor {
        once(plugin_connect_peer(&self.0))
    }
    specifier_boilerplate!(noglobalstate singleconnect no_subspec );
}
specifier_class!(
    name = PluginConnectClass,
    target = PluginConnect,
    prefixes = ["plugin-connect:", "plugin-c:"],
    arg_handling = parse,
    overlay = false,
    StreamOriented,
    SingleConnect,
    help = r#"
Load dynamic library specified by --plugin and pass the rest of command line argument
to the plugin code. Is not a listener and is not an overlay.

Example: listen for websocket connections and let the plugin do the talking

    websocat --binary ws-l:127.0.0.1:1234 plugin-connect: --plugin ./libwebsocat_plugin_example.so
"#
);


pub fn plugin_connect_peer(arg: &str) -> BoxedNewPeerFuture {
    let lib = match Library::new("./yes.so") {
        Ok(x) => x,
        Err(e) => return peer_err(e),
    };
    
    let (s_requests,requests) = channel(0);
    let (read_result,r_read) = channel(0);
    let (write_result,r_write) = channel(0);
    
    let read_buffer  = Arc::new(UnsafeCell::new(Vec::with_capacity(1024))); 
    let write_buffer = Arc::new(UnsafeCell::new(Vec::with_capacity(1024))); 
    
    
    let t = PluginThread {
        lib,
        requests,
        read_result,
        write_result,
        read_buffer: read_buffer.clone(),
        write_buffer: write_buffer.clone(),
    };
    
    let _ = ::std::thread::spawn(move || {
        if let Err(e) = t.run() {
            error!("plugin error: {}", e);
        } else {
            info!("plugin thread finished");
        }
    });
    
    let pr = PluginRead {
        request: Some(SenderState::Idle(s_requests.clone())),
        read_result: r_read,
        read_buffer,
    };
    let pw = PluginWrite {
        request: Some(SenderState::Idle(s_requests)),
        write_result: r_write,
        write_buffer,
    };

    Box::new(ok(Peer::new(pr,pw)))
}


#[derive(Debug)]
enum ToSyncPlugin {
    Read (usize),
    Write(usize),
}



/// I'm not sure what to write here
/// TODO
unsafe impl Send for PluginThread{}

struct PluginThread {
    lib: Library,
    requests : Receiver<ToSyncPlugin>,
    read_result: Sender<usize>,
    write_result: Sender<usize>,
    
    read_buffer: Arc<UnsafeCell<Vec<u8>>>,
    write_buffer: Arc<UnsafeCell<Vec<u8>>>,
}


enum SenderState<T> {
    Idle(Sender<T>),
    InProgress(SinkSend<Sender<T>>),
    RequestSent(Sender<T>),
}

struct PluginRead {
    request : Option<SenderState<ToSyncPlugin>>,
    read_result: Receiver<usize>,
    read_buffer: Arc<UnsafeCell<Vec<u8>>>,
}
struct PluginWrite {
    request : Option<SenderState<ToSyncPlugin>>,
    write_result: Receiver<usize>,
    write_buffer: Arc<UnsafeCell<Vec<u8>>>,
}


impl PluginThread {
    fn run(self) -> Result<()> {
        macro_rules! initsym  {
            ($x:ident) => {
                let $x;
                unsafe {
                    let q : Symbol<plugin_api::$x>;
                    q = self.lib.get(concat!(stringify!($x),"\0").as_bytes())?;
                    if let Some(x) = q.lift_option() {
                        $x = x;
                    } else {
                        return Err(concat!("plugin's ",stringify!($x)," symbol points to NULL?"))?;
                    };
                }
            };
        }
        
        initsym!(websocat_api_version);
        
        if unsafe{websocat_api_version()} != plugin_api::WEBSOCAT_API_VERSION {
            Err("Plugin API version mismatch")?;
        }
        
        initsym!(websocat_create_connection);
        initsym!(websocat_destroy_connection);
        initsym!(websocat_read);
        initsym!(websocat_write);
        
        let arg = CStr::from_bytes_with_nul(b"qwerty\0").unwrap();
        let endpoint = unsafe{websocat_create_connection(arg.as_ptr())};
        
        let requests : Receiver<ToSyncPlugin> = self.requests;
        let mut read_ret:  Sender<usize>   = self.read_result;
        let mut write_ret: Sender<usize>   = self.write_result;
        
        let mut requests = requests.wait();
        while let Some(Ok(rq)) = requests.next() {
            debug!("request: {:?}", rq);
            match rq {
                ToSyncPlugin::Read(len) => {
                    let buf = unsafe{&mut *self.read_buffer.get()};
                    buf.reserve(len);
                    unsafe{buf.set_len(len)};
                    let buf = buf.as_mut_ptr() as *mut c_void;
                    let ret = unsafe{websocat_read(endpoint, buf, len as u32)};
                    if (ret<0) {
                        unimplemented!();
                    }
                    if read_ret.try_send(ret as usize).is_err() { break };
                },
                ToSyncPlugin::Write(len) => {
                    let buf = unsafe{&mut *self.write_buffer.get()}.as_mut_ptr() as *mut c_void;
                    let ret = unsafe{websocat_write(endpoint, buf, len as u32)};
                    if (ret<=0) {
                        unimplemented!();
                    }
                    if write_ret.try_send(ret as usize).is_err() { break };
                },
            };
        };
        
        unsafe{websocat_destroy_connection(endpoint)};
        
        Ok(())
    }
}

macro_rules! read_or_write {
    ($self:expr, $typ:expr, $rr:expr, $cmd:expr, pre=$pre:block, post($l:ident)=$post:block) => {{
        use self::SenderState::{Idle,InProgress,RequestSent};
        
        trace!($typ);
        loop {
            match $self.request.take().unwrap() {
                Idle(s) => {
                    trace!(concat!($typ," Idle"));
                    let rq = $cmd;
                    $self.request = Some(InProgress(s.send(rq)));
                },
                InProgress(mut ss) => {
                    match ss.poll() {
                        Ok(Async::Ready(s)) => {
                            trace!(concat!($typ," InProgress Ready"));
                            $pre;
                            $self.request = Some(RequestSent(s));
                        },
                        Ok(Async::NotReady) => {
                            trace!(concat!($typ," InProgress NotReady"));
                            $self.request = Some(InProgress(ss));
                            return wouldblock();
                        },
                        Err(_) => {
                            warn!(concat!($typ," InProgress Err"));
                            $self.request = Some(InProgress(ss));
                            return Err(simple_err("pipe failed".to_string()));
                        },
                    }
                },
                RequestSent(s) => {
                    // simulating Future::and_then manually
                    
                    match $rr.poll() {
                        Ok(Async::Ready(Some($l))) => {
                            trace!(concat!($typ," RequestSent Ready(Some)"));
                            $self.request = Some(Idle(s));
                            $post;
                            return Ok($l);
                        }
                        Ok(Async::NotReady) => {
                            trace!(concat!($typ," RequestSent NotReady"));
                            $self.request = Some(RequestSent(s));
                            return wouldblock();
                        }
                        Ok(Async::Ready(None)) => {
                            warn!(concat!($typ," RequestSent Ready(None)"));
                            $self.request = Some(Idle(s));
                            return Err(simple_err("pipe failed 2".to_string()));
                        }
                        Err(_) => {
                            warn!(concat!($typ," RequestSent Err"));
                            $self.request = Some(RequestSent(s));
                            return Err(simple_err("pipe failed 3".to_string()));
                        }
                    }
                }
            }
        }
    }};
}

impl Read for PluginRead {
    fn read(&mut self, buf: &mut [u8]) -> ::std::result::Result<usize, ::std::io::Error> {
        read_or_write!(
            self,
            "read", 
            self.read_result,
            ToSyncPlugin::Read(buf.len()),
            pre={},
            post(l)={
                let b = unsafe{&mut *self.read_buffer.get()};
                if buf.len() < l {
                    error!("Buffer suddenly shrunk");
                }
                let minlen = buf.len().min(l);
                buf[0..minlen].copy_from_slice(&b[0..minlen]);
            }
        )
    }
}
impl AsyncRead for PluginRead{}



impl Write for PluginWrite {
    fn write(&mut self, buf: &[u8]) -> ::std::result::Result<usize, ::std::io::Error> {
        read_or_write!(
            self,
            "write", 
            self.write_result,
            ToSyncPlugin::Write(buf.len()),
            pre={
                let b = unsafe{&mut *self.write_buffer.get()};
                b.clear();
                b.extend_from_slice(buf);
            },
            post(l)={}
        )
    }
    fn flush(&mut self) -> ::std::result::Result<(), ::std::io::Error> {
        Ok(())
    }
}
impl AsyncWrite for PluginWrite {
    fn shutdown(&mut self) -> ::std::result::Result<Async<()>, ::std::io::Error> {
        Ok(Async::Ready(()))
    }
}
