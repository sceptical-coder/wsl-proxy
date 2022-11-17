mod essentials;
mod wsl_proxy;
mod peers;
mod receivers;

use tokio::sync::oneshot;
use futures::{try_join, FutureExt};
use tokio_util::codec::{FramedRead, BytesCodec};
use std::iter::once;
use std::path::PathBuf;
use std::sync::Arc;
use peers::*;
use receivers::*;

#[derive(Debug, clap::Parser)]
struct Cli {
    port_file: PathBuf,
    server_path: PathBuf,
    inferior: Vec<OsString>,
}

#[tracing::instrument]
pub(crate) async fn provide_server_streams(port: u16, peer: CommunicationPeer) -> Result<ProxyStreams, Error> {
   let mut chans = EnumMap::<StreamDir, Option<oneshot::Sender<Residual>>>::default();

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

         let str = connect_to_daemon(port, peer).await?;

         trace!("Connected to daemon with {:#?}", str);

         trace!("Before getting stream ID");
         let (str_in, str_out) = str.into_split();
         let (stream_dir, str_in, ) = fork_to_value(str_in).await?;
         let stream_dir = match stream_dir {
            None => break Err::<(), Error>(anyhow::anyhow!("Server sends jibberish!!!")),
            Some(stream_dir) => stream_dir,
         };
         trace!("Got stream ID: {:#?}", stream_dir);
         let chan = match chans[stream_dir].take() {
            None => break Err(anyhow::anyhow!("Channel to send to is not created yet!!!")),
            Some(chan) => chan
         };
         trace!("Before sending stream to awaiter");
         let send_res = chan.send(
            (Box::new(str_in), Box::new(str_out))
         );
         trace!("After sending stream to awaiter");
         match send_res {
            Ok(()) => (),
            Err(_) => break Err(anyhow::anyhow!("Channel to send to was already attached!")),
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
         kv.into_iter().map(|(dir, str)| (dir, Some(str)))
         .collect::<EnumMap<_, _>>()))
   }
}

#[tracing::instrument]
#[tokio::main(flavor = "current_thread")]
async fn try_main() -> Result<Infallible, Error> {
    let Cli{port_file, server_path, inferior: inferior_cmd_line} = get_args()?;

   trace!("inferior_command_line: {:?}", inferior_cmd_line);
   let (server, ProxyStreams(mut streams)) = {
      let port_file = std::fs::File::open(canonicalize_port_file(port_file)?)?;
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
         .chain(once(port.to_string().into()))
         .chain(once(pid.to_string().into()))
         .chain(once("--".into()))
         .chain(inferior_cmd_line.into_iter())
         .collect::<Vec<_>>();
      trace!("Starting server: {:?}", server_cmd_line);
      let server = tokio::process::Command::from(setup_process(&server_cmd_line, || Stdio::null())?).spawn()?;
      trace!("Server started: {:#?}", server);
      let server_streams = server_streams_fut.await?;
      trace!("Got server streams: {:#?}", server_streams);
      (server, server_streams)
   };

   exit(proxy_child(
      server,
      ProvidedStreams {
         stdin: Some(Box::new(FramedRead::new(tokio::io::stdin(), BytesCodec::new()))),
         stdout: Some(Box::new(tokio::io::stdout())),
         stderr: Some(Box::new(tokio::io::stderr())),
      },
      ControlledStreams {
         stdin: Some(Box::new(streams[StreamDir::Stdin].take().unwrap().1)),
         stdout: Some(streams[StreamDir::Stdout].take().unwrap().0),
         stderr: Some(streams[StreamDir::Stderr].take().unwrap().0),
      },
   ).await?)
}

fn main() {
   master(pid_log("client"), try_main)
}