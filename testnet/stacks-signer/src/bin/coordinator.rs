use clap::Parser;

use stacks_signer::config::Config;
use stacks_signer::net::{self, HttpNet, HttpNetError, Message, Net};
use stacks_signer::signing_round::{DkgBegin, MessageTypes};

const DEVNET_COORDINATOR_ID: usize = 0;
const DEVNET_COORDINATOR_DKG_ID: [u8; 32] = [0; 32];

fn main() {
    let cli = Cli::parse();
    let config = Config::from_file("conf/stacker.toml").unwrap();

    let net: HttpNet = HttpNet::new(config.common.stacks_node_url.clone(), vec![]);
    let mut coordinator = Coordinator::new(DEVNET_COORDINATOR_ID, DEVNET_COORDINATOR_DKG_ID, net);

    coordinator
        .run(&cli.command)
        .expect("Failed to execute command");
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand, Debug)]
enum Command {
    Dkg,
    Sign { msg: String },
    GetAggregatePublicKey,
}

#[derive(Debug)]
struct Coordinator<Network: Net> {
    id: usize,        // Used for relay coordination
    dkg_id: [u8; 32], // TODO: Is this a public key?
    network: Network,
}

impl<Network: Net> Coordinator<Network> {
    fn new(id: usize, public_key: [u8; 32], network: Network) -> Self {
        Self {
            id,
            dkg_id: public_key,
            network,
        }
    }
}

impl<Network: Net> Coordinator<Network>
where
    Error: From<Network::Error>,
{
    pub fn run(&mut self, command: &Command) -> Result<(), Error> {
        match command {
            Command::Dkg => self.run_distributed_key_generation(),
            Command::Sign { msg } => self.sign_message(msg),
            Command::GetAggregatePublicKey => self.get_aggregate_public_key(),
        }
    }

    pub fn run_distributed_key_generation(&mut self) -> Result<(), Error> {
        let dkg_begin_message = Message {
            msg: MessageTypes::DkgBegin(DkgBegin { id: self.dkg_id }),
            sig: net::id_to_sig_bytes(self.id),
        };

        self.network.send_message(dkg_begin_message)?;

        self.wait_for_dkg_end(10) // TODO: Should be number of signers + some delta
    }

    pub fn sign_message(&mut self, msg: &str) -> Result<(), Error> {
        todo!();
    }

    pub fn get_aggregate_public_key(&mut self) -> Result<(), Error> {
        todo!();
    }

    fn wait_for_dkg_end(&mut self, mut attempts: usize) -> Result<(), Error> {
        loop {
            match (attempts, self.wait_for_next_message()?.msg) {
                (0, _) => return Err(Error::MaxAttemptsReached),
                (_, MessageTypes::DkgEnd) => return Ok(()),
                (_, _) => attempts -= 1,
            }
        }
    }

    fn wait_for_next_message(&mut self) -> Result<Message, Error> {
        let get_next_message = || {
            self.network.poll(self.id);
            self.network
                .next_message()
                .ok_or("No message yet".to_owned())
                .map_err(backoff::Error::transient)
        };

        let notify = |_err, dur| {
            println!("No message at {:?}, retrying again soon", dur);
        };

        backoff::retry_notify(
            backoff::ExponentialBackoff::default(),
            get_next_message,
            notify,
        )
        .map_err(|_| Error::Timeout)
    }
}

#[derive(thiserror::Error, Debug)]
enum Error {
    #[error("Http network error: {0}")]
    NetworkError(#[from] HttpNetError),

    #[error("Operation timed out")]
    Timeout,

    #[error("Maximum attempts waiting for message reached")]
    MaxAttemptsReached,
}
