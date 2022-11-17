#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use anyhow;
use bytes::Buf;
use enum_map::{Enum, EnumMap};
use futures::future::{try_join_all};
use futures::{try_join};
use futures::{prelude::*};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Child;
use tokio::sync::oneshot;
use tracing::{trace};
use std::convert::Infallible;
use std::ffi::OsString;
use std::iter::once;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::{Arc};
use std::task::Poll;
use std::{process::Stdio};
use std::result::Result;
use clap::{self, Parser};
use tokio::net::TcpStream;
use serde;
use tokio_serde::{SymmetricallyFramed, formats::SymmetricalMessagePack};
use tokio_util::codec::{FramedRead, LengthDelimitedCodec};
use strum_macros;
use strum::IntoEnumIterator;
use fs2::FileExt;

use tokio_util::codec::FramedWrite;

type Error = anyhow::Error;

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone, Copy, strum_macros::EnumIter, Enum)]
enum StreamDir {
   Stdin,
   Stdout,
   Stderr,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone, Copy, PartialEq)]
enum CommunicationSide {
   Client,
   Server,
}

type CommmunicationId = u32;

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone, Copy)]
struct CommunicationPeer {
   side: CommunicationSide,
   id: CommmunicationId,
}

#[derive(Debug, clap::Subcommand)]
enum PeerConfig {
   Server{listener_port: u16, client_pid: CommmunicationId, inferior: Vec<OsString>},
   Client{server_path: PathBuf, inferior: Vec<OsString>},
   #[cfg(unix)]
   Daemon,
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
   #[command(subcommand)]
   peer_config: PeerConfig,
}

#[derive(Default)]
struct ProvidedStreams {
   stdin: Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>>,
   stdout: Option<Box<dyn tokio::io::AsyncWrite + Send + Unpin>>,
   stderr: Option<Box<dyn tokio::io::AsyncWrite + Send + Unpin>>,
}

#[derive(Default)]
struct ControlledStreams {
   stdin: Option<Box<dyn tokio::io::AsyncWrite + Send + Unpin>>,
   stdout: Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>>,
   stderr: Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>>,
}

#[derive(Debug, Default)]
struct ProxyStreams(EnumMap<StreamDir, Option<Box<TcpStream>>>);

async fn connect_to_daemon(port: u16, peer: CommunicationPeer) -> anyhow::Result<TcpStream> {
   trace!("Opening unmarked connection to daemon for peer: {:#?}", peer);
   let mut stream = TcpStream::connect(("127.0.0.1", port)).await?;
   trace!("Opened unmarked connection for stream dir: {:#?}", peer);
   let length_delimited = FramedWrite::new(&mut stream, LengthDelimitedCodec::new());
   let mut serialized =
      SymmetricallyFramed::new(length_delimited, SymmetricalMessagePack::<CommunicationPeer>::default());
   serialized.send(peer).await?;
   trace!("Marked connection to daemon for peer: {:#?}", peer);
   Ok(stream)
}

async fn connect_to_client(mut stream: TcpStream, stream_target: StreamDir)
   -> Result<TcpStream, Error>
{
   let length_delimited = FramedWrite::new(&mut stream, LengthDelimitedCodec::new());
   let mut serialized =
      SymmetricallyFramed::new(length_delimited, SymmetricalMessagePack::<StreamDir>::default());
   serialized.send(stream_target).await?;
   trace!("Marked connection for stream dir: {:#?}", stream_target);
   Ok(stream)
}

fn flatten<T, E, F>(res: Result<Result<T, E>, F>) -> Result<T, Error>
where
   E: Into<Error>,
   F: Into<Error>,
{
   match res {
      Ok(Ok(result)) => Ok::<T, Error>(result),
      Ok(Err(err)) => Err(err.into()),
      Err(err) => Err(err.into()),
  }
}

async fn proxy<R, W>(mut reader: Box<R>, mut writer: Box<W>) -> Result<(), std::io::Error>
where
   R: tokio::io::AsyncRead + Send + 'static + ?Sized + Unpin,
   W: tokio::io::AsyncWrite + Send + 'static + ?Sized + Unpin,
{
   let mut buf = vec![0; 1usize << 20];
   loop {
      match reader.read(&mut buf).await? {
         0 => return writer.shutdown().await,
         n => {
            let mut read = std::io::Cursor::new(&mut buf[..n]);
            loop {
               if !read.has_remaining() {
                  break;
               }
               match writer.write_buf(&mut read).await? {
                  0 => return Ok(()),
                  _ => (),
               }
            }
         }
      }
   }
}

async fn proxy_child(mut child: Child, mut provided_streams: ProvidedStreams, mut controlled_streams: ControlledStreams) -> anyhow::Result<ExitStatus> {
   let streams_fut = tokio::spawn(async move {
      try_join! (
         proxy(provided_streams.stdin.take().unwrap(), controlled_streams.stdin.take().unwrap()),
         proxy(controlled_streams.stdout.take().unwrap(), provided_streams.stdout.take().unwrap()),
         proxy(controlled_streams.stderr.take().unwrap(), provided_streams.stderr.take().unwrap()),
      )
   }.fuse());
   let res = child.wait().await?;
   if let Poll::Ready(r) = futures::poll!(streams_fut) {
      let _ = flatten(r)?;
   }
   Ok(res)
}

#[tracing::instrument]
async fn start_inferior(command_line: &Vec<OsString>, ProxyStreams(mut streams): ProxyStreams) -> anyhow::Result<ExitStatus>
{
   #[cfg(windows)]
   #[cfg(create_desktop)]
   {
      use windows::Win32::System::Threading::{GetCurrentThreadId};
      use windows::{
         core::*, Win32::{System::{StationsAndDesktops::*, SystemServices::*}, Security::SECURITY_ATTRIBUTES},
      };
      use windows::Win32::Foundation::GetLastError;
      use windows::Win32::Foundation::BOOL;
      #[macro_use(defer)]
      extern crate scopeguard;

      let old_desktop = unsafe { GetThreadDesktop(GetCurrentThreadId()) }?;

      let desktop = unsafe
      {
         let mut sa = SECURITY_ATTRIBUTES::default();
         sa.bInheritHandle = BOOL(1);
         CreateDesktopW(
            w!("hiddenDesktop"),
            PCWSTR::null(),
            None,
            Default::default(),
            DESKTOP_CREATEWINDOW.0,
            Some(&sa),
         )?
      };
      match desktop {
         HDESK(ptr) if ptr == 0 => anyhow::bail!(anyhow::Error::new(windows::core::Error::from(unsafe{ GetLastError() })).context("Failed to open desktop!!!")),
         HDESK(_) => ()
      }
      defer! {
         unsafe { CloseDesktop(desktop); }
      }
      match unsafe { SetThreadDesktop(desktop) } {
         BOOL(0) => anyhow::bail!(anyhow::Error::new(windows::core::Error::from(unsafe{ GetLastError() })).context("Failed to assign desktop!!!")),
         _ => ()
      }
      match unsafe { CloseDesktop(old_desktop) } {
         BOOL(0) => anyhow::bail!(anyhow::Error::new(windows::core::Error::from(unsafe{ GetLastError() })).context("Failed to close old desktop!!!")),
         _ => ()
      }
   }

   #[cfg(not(windows))]
   let hider = std::convert::identity;
   #[cfg(windows)]
   let hider = |mut cmd: std::process::Command| {
      use windows::Win32::System::Threading::CREATE_NO_WINDOW;
      use std::os::windows::process::CommandExt;
      cmd.creation_flags(CREATE_NO_WINDOW.0);
      cmd
   };
   let inferior_cmd = hider(setup_process(command_line, || Stdio::piped())?);
   let mut inferior = tokio::process::Command::from(inferior_cmd).spawn()?;
   let controlled_streams = ControlledStreams{
      stdin: Some(Box::new(inferior.stdin.take().unwrap())),
      stdout: Some(Box::new(inferior.stdout.take().unwrap())),
      stderr: Some(Box::new(inferior.stderr.take().unwrap())),
   };
   proxy_child(
      inferior,
      ProvidedStreams{
         stdin: Some(Box::new(streams[StreamDir::Stdin].take().unwrap())),
         stdout: Some(Box::new(streams[StreamDir::Stdout].take().unwrap())),
         stderr: Some(Box::new(streams[StreamDir::Stderr].take().unwrap())),
      },
      controlled_streams,
   ).await
}

fn setup_process(cmd_line: &Vec<OsString>, io: fn() -> Stdio) -> Result<std::process::Command, Error>
{
   let mut res = std::process::Command
      ::new(cmd_line.first().ok_or(anyhow::anyhow!("Expected to have non-empty command-line for inferior"))?);
   res
      .args(&cmd_line[1..])
      .stdin(io())
      .stdout(io())
      .stderr(io());
   Ok(res)
}

#[tracing::instrument]
async fn provide_server_streams(port: u16, peer: CommunicationPeer) -> Result<ProxyStreams, Error> {
   let mut chans = EnumMap::<StreamDir, Option<oneshot::Sender<TcpStream>>>::default();

   let stop_accept_token = tokio_util::sync::CancellationToken::new();
   let accepted_all = stop_accept_token.clone();

   let awaiters_fut =
   {
      let accept_drop_guard = Arc::new(stop_accept_token.drop_guard());
      let res = try_join_all(StreamDir::iter()
         .map(|dir| {
            let (sender, receiver) = oneshot::channel();
            chans[dir] = Some(sender);

            let accept_guard = accept_drop_guard.clone();
            async move {
               let stream = receiver.await
                  .or_else(|_|{anyhow::bail!("Unable to receive socket for reading!!!")})?;
               trace!("Got incoming stream: {:#?}", stream);
               drop(accept_guard);
               Ok((dir, stream))
            }
         }));
      res
   };

   let acceptor = async move {
      loop {
         trace!("Connecting to daemon at {:?}", port);

         let mut str = connect_to_daemon(port, peer).await?;

         trace!("Connected to daemon at {:?}", port);

         let length_delimited = FramedRead::new(&mut str, LengthDelimitedCodec::new());
         let mut deserialized = SymmetricallyFramed::new(length_delimited, SymmetricalMessagePack::<StreamDir>::default());
         trace!("Before getting stream ID");
         match { let res = deserialized.try_next().await?; trace!("Got stream ID: {:#?}", res); res } {
            None => break Err::<(), Error>(anyhow::anyhow!("Server sends jibberish!!!")),
            Some(stream_dir) => match chans[stream_dir].take() {
               None => break Err(anyhow::anyhow!("Channel to send to is not created yet!!!")),
               Some(chan) =>
               {
                  trace!("Before sending stream to awaiter");
                  let send_res = chan.send(str);
                  trace!("After sending stream to awaiter");
                  match send_res {
                     Ok(()) => (),
                     Err(_) => break Err(anyhow::anyhow!("Channel to send to was already attached!")),
                  }
               }
            }
         }
      }
   };

   let join_acceptor = tokio::spawn(async move {
      tokio::select! {
         _ = accepted_all.cancelled() => Ok::<(), Error>(()),
         Err(err) = acceptor => Err::<(), Error>(err),
      }
   });

   match try_join! (join_acceptor.map(flatten), awaiters_fut) {
      Err(err) => Err(err),
      Ok(((), kv)) => Ok(ProxyStreams(
         kv.into_iter().map(|(dir, str)| (dir, Some(Box::new(str))))
         .collect::<EnumMap<_, _>>()))
   }
}

#[tracing::instrument]
async fn client_main(server_path: PathBuf, inferior_cmd_line: Vec<OsString>) -> Result<Infallible, Error> {
   trace!("inferior_command_line: {:?}", inferior_cmd_line);
   let (server, ProxyStreams(mut streams)) = {
      let port_file = port_file_path()?;
      let port_file = std::fs::File::open(port_file)?;
      let port_file = scopeguard::guard(&port_file,
         |pf| {
            use fs2::FileExt;
            let _ = pf.unlock();
         });
      let port = read_port_file(&port_file)?;

      let pid = std::process::id();
      let server_streams_fut = tokio::spawn(
         provide_server_streams(port, CommunicationPeer { side: CommunicationSide::Client, id: pid })).map(flatten);
      let server_cmd_line =
         once(server_path.as_os_str().to_owned())
         .chain(once("server".to_owned().into()))
         .chain(once(port.to_string().into()))
         .chain(once(pid.to_string().into()))
         .chain(once("--".into()))
         .chain(inferior_cmd_line.into_iter())
         .collect::<Vec<_>>();
      trace!("Starting server: {:?}", server_cmd_line);
      let server = tokio::process::Command::from(setup_process(&server_cmd_line, || Stdio::inherit())?).spawn()?;
      trace!("Server started: {:#?}", server);
      let server_streams = server_streams_fut.await?;
      trace!("Got server streams: {:#?}", server_streams);
      (server, server_streams)
   };

   exit(proxy_child(
      server,
      ProvidedStreams {
         stdin: Some(Box::new(tokio::io::stdin())),
         stdout: Some(Box::new(tokio::io::stdout())),
         stderr: Some(Box::new(tokio::io::stderr())),
      },
      ControlledStreams {
         stdin: Some(Box::new(streams[StreamDir::Stdin].take().unwrap())),
         stdout: Some(Box::new(streams[StreamDir::Stdout].take().unwrap())),
         stderr: Some(Box::new(streams[StreamDir::Stderr].take().unwrap())),
      },
   ).await?)
}

#[tracing::instrument]
async fn server_main(listener_port: u16, client_pid: CommmunicationId, inferior_cmd_line: Vec<OsString>) -> Result<Infallible, Error> {
   trace!("Before loop that connects to client");
   let peer = CommunicationPeer{id: client_pid, side: CommunicationSide::Server};
   let cs = ProxyStreams(
      try_join_all(StreamDir::iter().map(
         |dir| tokio::spawn(async move {
            let stream = connect_to_daemon(listener_port, peer).await?;
            connect_to_client(stream, dir).await
         })
         .map(flatten)
         .map(move |res|
            res.map(|str| (dir, str)))
         ))
      .await?
      .into_iter()
      .map(|(k, v)| (k, Some(Box::new(v))))
      .collect::<EnumMap<_, _>>()
   );
   trace!("Connected to client, streams: {:#?}", cs);
   exit(start_inferior(&inferior_cmd_line, cs).await?)
}

#[cfg(not(unix))]
fn get_exit_code(e: ExitStatus) -> i32 {
   e.code().unwrap()
}

#[cfg(unix)]
fn get_exit_code(e: ExitStatus) -> i32 {
   use std::os::unix::process::ExitStatusExt;
   ExitStatusExt::into_raw(e)
}

fn exit(e: ExitStatus) -> ! {
   std::process::exit(get_exit_code(e))
}

fn port_file_path() -> anyhow::Result<PathBuf>
{
   let env_key = "XDG_RUNTIME_DIR";
   let filename = "wsl-proxy.port";
   let runtime_dir = std::env::var_os(env_key);
      
   let env_val = match runtime_dir {
      None => anyhow::bail!("{} environment variable is expected to be set!", env_key),
      Some(env_val) => PathBuf::from(env_val)
   };
   let mut env_val = std::fs::canonicalize(env_val)?;

   env_val.push(filename);
   Ok(env_val)
}

#[tokio::main]
async fn try_main() -> Result<Infallible, Error> {
   let args = Cli::try_parse()?;
   trace!("args: {:#?}", args);
   match args.peer_config {
      PeerConfig::Client{server_path, inferior} => client_main(server_path, inferior).await,
      PeerConfig::Server{listener_port, client_pid, inferior} => server_main(listener_port, client_pid, inferior).await,
      #[cfg(unix)]
      PeerConfig::Daemon => daemon_main().await,
   }
}

#[cfg(unix)]
mod daemon {
   use tokio::net::TcpListener;
   use std::collections::VecDeque;
   use std::collections::HashMap;
   use std::sync::Arc;
   use tokio::sync::Mutex;
   use crate::*;

   struct UnpairedSocks
   {
      side: CommunicationSide,
      head: TcpStream,
      tail: VecDeque<TcpStream>,
   }

   pub struct Listener
   {
      listener: TcpListener,
      unpaired_socks: Arc<Mutex<HashMap<CommmunicationId, Option<UnpairedSocks>>>>,
   }

   impl Listener {
      pub fn new(listener: TcpListener) -> Self {
         Self{ listener, unpaired_socks: Arc::new(Mutex::new(HashMap::new())) }
      }

      pub async fn accept(self: &Self, ) -> anyhow::Result<()> {
         let unpaired_socks = self.unpaired_socks.clone();
         let (mut incoming, sock_addr) = self.listener.accept().await?;

         tokio::spawn(async move{
            let length_delimited = FramedRead::new(&mut incoming, LengthDelimitedCodec::new());
            let mut deserialized = SymmetricallyFramed::new(
               length_delimited, SymmetricalMessagePack::<CommunicationPeer>::default());
            match deserialized.try_next().await? {
               None => anyhow::bail!("Someone sends jibberish on sock {}!!!", sock_addr),
               Some(peer)  => {
                  let mut unpaired_socks = unpaired_socks.lock().await;
                  let unpaired = unpaired_socks.entry(peer.id).or_default();
                  struct Pair {
                     incoming: TcpStream,
                     complement: TcpStream,
                  }
                  let (new_unpaired, pair) = match std::mem::replace(unpaired, None) {
                     Some(mut unpaired) if unpaired.side != peer.side =>
                        match unpaired.tail.pop_front() {
                           None => {
                              let complement = unpaired.head;
                              (None, Some(Pair{incoming, complement}))
                           }
                           Some(new_head) => {
                              let complement = std::mem::replace(&mut unpaired.head, new_head);
                              (Some(unpaired), Some(Pair{complement, incoming}))
                           }
                        }
                        Some(mut unpaired) => {
                           unpaired.tail.push_back(incoming);
                           (Some(unpaired), None)
                        }
                        None => {
                           (Some(UnpairedSocks{side: peer.side, head: incoming, tail: VecDeque::default()}), None)
                     }
                  };
                  *unpaired = new_unpaired;
                  match pair {
                     None => Ok(()),
                     Some(Pair{incoming, complement}) => {
                        let (inc_read, inc_write) = incoming.into_split();
                        let (compl_read, compl_write) = complement.into_split();
                        Ok(try_join!(
                           proxy(Box::new(inc_read), Box::new(compl_write)),
                           proxy(Box::new(compl_read), Box::new(inc_write)),
                        )
                        .map(|_|())?)
                     }
                  }
               }
            }
         });
         Ok(())
      }
   }
}

fn read_port_file(port_file: &std::fs::File) -> anyhow::Result<u16> {
   port_file.try_lock_shared()?;
   use std::io::BufRead;
   let current_port = std::io::BufReader::new(port_file)
      .lines()
      .map(|r| r.map_err(anyhow::Error::new))
      .map(|r|
         r.and_then(|l| Ok(l.parse::<u16>()?)))
      .next();
   match current_port {
      None => anyhow::bail!("Port file seems to be somehow empty"),
      Some(r) => Ok(r?)
   }
}

#[cfg(unix)]
async fn daemon_main() -> anyhow::Result<Infallible> {
   trace!("Daemon main");
   use tokio::net::TcpListener;
   match fork::daemon(true, true) {
      Err(r) => anyhow::bail!("Failed to fork, return value is ({})", r),
      Ok(fork::Fork::Child) => (),
      Ok(fork::Fork::Parent(_)) => std::process::exit(0),
   };
   let port_file_path = port_file_path()?;
   trace!("Port file path: {:?}", port_file_path);
   let mut port_file = std::fs::File::options().read(true).write(true).create_new(true)
      .open(port_file_path.clone())?;
   let (port, listener) = {
      let mut port_file = scopeguard::guard(&mut port_file,
         |pf| {
            let _ = pf.unlock();
         });
      port_file.try_lock_exclusive()?;
      let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
      let port = listener.local_addr()?.port();
      use std::io::Write;
      writeln!(port_file, "{}", port)?;
      use std::io::Seek;
      port_file.rewind()?;
      (port, listener)
   };
   if read_port_file(&port_file)? != port {
      anyhow::bail!("Failed to become the daemon that will service clients")
   }
   {
      let _delete_port_file = scopeguard::guard(
         port_file_path,
         |p| { let _ = std::fs::remove_file(p); }
      );
      let listener = daemon::Listener::new(listener);
      loop {
         tokio::select!{
            r = tokio::signal::ctrl_c() => break r?,
            r = listener.accept() => r?,
         }
      }
   }
   std::process::exit(0)
}

fn main()
{
   #[cfg(debug_assertions)]
   {
      let log_file = format!("trace-{}.log", std::process::id());
      let log_out = std::fs::File::create(log_file).ok().unwrap();
      let builder =
         tracing_subscriber::FmtSubscriber::builder()
         .with_ansi(false)
         .with_writer(std::sync::Mutex::new(log_out));
      let builder = builder.with_max_level(tracing::Level::TRACE);
      tracing::subscriber::set_global_default(builder.finish()).ok();
   }
   match try_main() {
      Err(err) => { trace!("{}", err); },
      _ => panic!("Should not have come to be here!")
   };
}