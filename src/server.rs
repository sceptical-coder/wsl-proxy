#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod essentials;
mod wsl_proxy;
mod peers;

use futures::FutureExt;
use tokio_util::codec::{FramedRead, BytesCodec};
use tracing::trace;
use peers::*;

#[derive(Debug, clap::Parser)]
struct Cli {
    listener_port: u16,
    client_pid: CommmunicationId,
    inferior: Vec<OsString>,
}

#[tracing::instrument]
pub(crate) async fn start_inferior(command_line: &Vec<OsString>, ProxyStreams(mut streams): ProxyStreams) -> anyhow::Result<ExitStatus>
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
      stdout: Some(Box::new(FramedRead::new(inferior.stdout.take().unwrap(), BytesCodec::new()))),
      stderr: Some(Box::new(FramedRead::new(inferior.stderr.take().unwrap(), BytesCodec::new()))),
   };
   proxy_child(
      inferior,
      ProvidedStreams{
         stdin: Some(streams[StreamDir::Stdin].take().unwrap().0),
         stdout: Some(streams[StreamDir::Stdout].take().unwrap().1),
         stderr: Some(streams[StreamDir::Stderr].take().unwrap().1),
      },
      controlled_streams,
   ).await
}

pub(crate) async fn connect_to_client(mut stream: TcpStream, stream_target: StreamDir)
   -> Result<TcpStream, Error>
{
   write_to_wire(&mut stream, &stream_target).await?;
   trace!("Marked connection for stream dir: {:#?}", stream_target);
   Ok(stream)
}

#[tokio::main]
async fn try_main() -> Result<Infallible, Error> {
   let Cli{listener_port, client_pid, inferior: inferior_cmd_line} = get_args()?;
   trace!("Before loop that connects to client");
   let peer = wsl_proxy::CommunicationPeer{id: client_pid, side: CommunicationSide::Server};
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
      .map(|(k, v)| {
         let (s_in, s_out) = v.into_split();
         let s_in = FramedRead::new(s_in, BytesCodec::new());
         let res: Residual = (Box::new(s_in), Box::new(s_out));
         (k, Some(res))
      })
      .collect::<EnumMap<_, _>>()
   );
   trace!("Connected to client, streams: {:#?}", cs);
   exit(start_inferior(&inferior_cmd_line, cs).await?)
}

fn main() {
   master(pid_log("server"), try_main)
}