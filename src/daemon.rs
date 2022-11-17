#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod essentials;
mod wsl_proxy;
mod receivers;

use tracing::trace;
use std::path::PathBuf;
use crate::wsl_proxy::*;
use crate::receivers::*;

mod daemon {
    use tokio::net::TcpListener;
    use std::collections::VecDeque;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use futures::try_join;
    use super::*;
 
    #[derive(Debug)]
    struct UnpairedSocks
    {
       side: CommunicationSide,
       head: Residual,
       tail: VecDeque<Residual>,
    }
    #[derive(Debug)]
    pub struct Listener
    {
       listener: TcpListener,
       unpaired_socks: Arc<Mutex<HashMap<CommmunicationId, Option<UnpairedSocks>>>>,
    }
 
    impl Listener {
       pub fn new(listener: TcpListener) -> Self {
          Self{ listener, unpaired_socks: Arc::new(Mutex::new(HashMap::new())) }
       }
 
       #[tracing::instrument]
       pub async fn accept(self: &Self, ) -> anyhow::Result<()> {
          let unpaired_socks = self.unpaired_socks.clone();
          trace!("Accepting connection...");
          let (incoming, sock_addr) = self.listener.accept().await?;
          trace!("Accepted connection: {:#?}", (&incoming, &sock_addr));
 
          tokio::spawn(async move{
            let (incoming, descriptor): (Residual, _) = {   
               let (inc_in, inc_out) = incoming.into_split();
               let (descriptor, inc_in) = fork_to_value(inc_in).await?;
               ((Box::new(inc_in), Box::new(inc_out)), descriptor)
            };
            let peer: CommunicationPeer = match descriptor {
               None => anyhow::bail!("Someone sends jibberish on sock {}!!!", sock_addr),
               Some(r) => r,
            };
            trace!("Got descriptor {:#?} from conn {:#?}", peer, incoming);
            struct Pair {
               incoming: Residual,
               complement: Residual,
            }
            let pair = {
               let mut unpaired_socks = unpaired_socks.lock().await;
               let unpaired = unpaired_socks.entry(peer.id).or_default();
               let (new_unpaired, pair) = match std::mem::take(unpaired) {
                  Some(mut unpaired) if unpaired.side != peer.side =>
                     match unpaired.tail.pop_front() {
                        None => {
                           let complement = unpaired.head;
                           trace!("Pairing last unpaired connection!");
                           (None, Some(Pair{incoming, complement}))
                        }
                        Some(new_head) => {
                           let complement = std::mem::replace(&mut unpaired.head, new_head);
                           trace!("Pairing unpaired connection, some are still awaiting for that!");
                           (Some(unpaired), Some(Pair{complement, incoming}))
                        }
                     }
                  Some(mut unpaired) => {
                     trace!("Emplacing successive unpaired connection to queue!");
                     unpaired.tail.push_back(incoming);
                     (Some(unpaired), None)
                  }
                  None => {
                     trace!("Emplacing first unpaired connection!");
                     (Some(UnpairedSocks{side: peer.side, head: incoming, tail: VecDeque::default()}), None)
                  }
               };
               *unpaired = new_unpaired;
               pair
            };
            let Pair{incoming, complement} =  match pair {
               None => return Ok(()),
               Some(p) => p
            };
            trace!("Pairing {:#?} with {:#?}...", incoming, complement);
            let (inc_read, inc_write) = incoming;
            let (compl_read, compl_write) = complement;
            Ok(try_join!(
               proxy_residual(inc_read, compl_write),
               proxy_residual(compl_read, inc_write),
            )
            .map(|_|())?)
         });
         Ok(())
       }
    }
 }
 
 #[derive(Debug, clap::Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
   port_file: PathBuf,
}

#[tokio::main]
async fn async_main() -> Result<Infallible, Error>
{
   let Cli{port_file: port_file_path} = get_args()?;
   let port_file_path = canonicalize_port_file(port_file_path)?;

   trace!("Port file path: {:?}", port_file_path);
   trace!("");
   let mut port_file = std::fs::File::options().read(true).write(true).create(true)
      .open(&port_file_path)?;
   trace!("");
   let (port, listener) = {
      trace!("");
      let mut port_file = scopeguard::guard(&mut port_file,
         |pf| {
            let _ = pf.unlock();
         });
         trace!("");
      port_file.try_lock_exclusive()?;
      trace!("Binding listener...");
      let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
      let port = listener.local_addr()?.port();
      trace!("Bound listener at {}: {:#?}", port, listener);
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
         move |p| {
            drop(port_file);
            let _ = std::fs::remove_file(p);
         }
      );
      let listener = daemon::Listener::new(listener);
      loop { 
         listener.accept().await?
      }
   }
}

fn try_main() -> Result<Infallible, Error>
{
   #[cfg(unix)]
   match fork::daemon(true, true) {
      Err(r) => anyhow::bail!("Failed to fork, return value is ({})", r),
      Ok(fork::Fork::Child) => (),
      Ok(fork::Fork::Parent(_)) => std::process::exit(0),
   };
   async_main()
}

fn main() {
   master(pid_log("daemon"), try_main);
}