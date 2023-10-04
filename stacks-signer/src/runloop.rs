use std::collections::VecDeque;
use std::sync::mpsc::Sender;
use std::time::Duration;

use libsigner::{SignerRunLoop, StackerDBChunksEvent};
use p256k1::ecdsa;
use slog::{slog_debug, slog_error, slog_info, slog_warn};
use stacks_common::{debug, error, info, warn};
use wsts::common::MerkleRoot;
use wsts::net::{Message, Packet, Signable};
use wsts::state_machine::coordinator::frost::Coordinator as FrostCoordinator;
use wsts::state_machine::coordinator::Coordinatable;
use wsts::state_machine::signer::SigningRound;
use wsts::state_machine::{OperationResult, PublicKeys};
use wsts::v2;

use crate::config::Config;
use crate::stacks_client::StacksClient;

/// Which operation to perform
#[derive(PartialEq, Clone)]
pub enum RunLoopCommand {
    /// Generate a DKG aggregate public key
    Dkg,
    /// Sign a message
    Sign {
        /// The bytes to sign
        message: Vec<u8>,
        /// Whether to make a taproot signature
        is_taproot: bool,
        /// Taproot merkle root
        merkle_root: Option<MerkleRoot>,
    },
}

/// The RunLoop state
#[derive(PartialEq, Debug)]
pub enum State {
    /// The runloop is idle
    Idle,
    /// The runloop is executing a DKG round
    Dkg,
    /// The runloop is executing a signing round
    Sign,
}

/// The runloop for the stacks signer
pub struct RunLoop<C> {
    /// The timeout for events
    pub event_timeout: Duration,
    /// The coordinator for inbound messages
    pub coordinator: C,
    /// The signing round used to sign messages
    // TODO: update this to use frost_signer directly instead of the frost signing round
    // See: https://github.com/stacks-network/stacks-blockchain/issues/3913
    pub signing_round: SigningRound<v2::Signer>,
    /// The stacks client
    pub stacks_client: StacksClient,
    /// Received Commands that need to be processed
    pub commands: VecDeque<RunLoopCommand>,
    /// The current state
    pub state: State,
}

impl<C: Coordinatable> RunLoop<C> {
    /// Helper function to check if we need to run DKG
    fn should_run_dkg(&self) -> bool {
        if self.state != State::Idle {
            // We can't do anything unless we are idle...will check again later (we may already be running a DKG round...)
            return false;
        }
        // Determine if we are the coordinator
        let (coordinator_id, _) = calculate_coordinator(&self.signing_round.public_keys);
        if let Some(key) = self.coordinator.get_aggregate_public_key() {
            // We have an aggregate public key. Check if we need to cast our vote
            // TODO: Add state to keep track of what contract calls we have made so we don't recast our vote needlessly and force blocks to include multiple of the same stx transactions
            // TODO: should I use ok() here or log some error?
            if self.stacks_client.get_aggregate_public_key().ok().is_none() {
                // Note this is written under the assumption that if no conensus is reached in the contract, the pox contract will flush all votes and start over
                match self.stacks_client.get_aggregate_public_key_vote() {
                    Ok(Some(key)) => {
                        // We have already voted for an aggregate public key. We are done!
                        debug!("Already voted for aggregate public key: {:?}", key);
                    }
                    Ok(None) => {
                        // No aggregate public key has been set yet. We need to vote for it!
                        debug!("Voting for aggregate public key: {:?}", key);
                        if let Err(e) = self.stacks_client.cast_aggregate_public_key_vote(key) {
                            // TODO: are reattempts handled within stacks client or are they handled here?
                            error!("Failed to cast aggregate public key vote: {:?}", e);
                        }
                    }
                    Err(e) => {
                        error!("Failed to get aggregate public key vote: {:?}", e);
                    }
                }
            }
            false
        } else {
            // If we are the coordinator and the DKG command is not already queued, we should queue it
            coordinator_id == self.signing_round.signer_id
                && self.commands.front() != Some(&RunLoopCommand::Dkg)
        }
    }

    /// Helper function to actually execute the command and update state accordingly
    /// Returns true when it is successfully executed, else false
    fn execute_command(&mut self, command: &RunLoopCommand) -> bool {
        match command {
            RunLoopCommand::Dkg => {
                info!("Starting DKG");
                match self.coordinator.start_distributed_key_generation() {
                    Ok(msg) => {
                        let ack = self
                            .stacks_client
                            .send_message(self.signing_round.signer_id, msg);
                        debug!("ACK: {:?}", ack);
                        self.state = State::Dkg;
                        true
                    }
                    Err(e) => {
                        error!("Failed to start DKG: {:?}", e);
                        warn!("Resetting coordinator's internal state.");
                        self.coordinator.reset();
                        false
                    }
                }
            }
            RunLoopCommand::Sign {
                message,
                is_taproot,
                merkle_root,
            } => {
                info!("Signing message: {:?}", message);
                match self
                    .coordinator
                    .start_signing_message(message, *is_taproot, *merkle_root)
                {
                    Ok(msg) => {
                        let ack = self
                            .stacks_client
                            .send_message(self.signing_round.signer_id, msg);
                        debug!("ACK: {:?}", ack);
                        self.state = State::Sign;
                        true
                    }
                    Err(e) => {
                        error!("Failed to start signing message: {:?}", e);
                        warn!("Resetting coordinator's internal state.");
                        self.coordinator.reset();
                        false
                    }
                }
            }
        }
    }

    /// Helper function to check the current state, process the next command in the queue, and update state accordingly
    fn process_next_command(&mut self) {
        match self.state {
            State::Idle => {
                if let Some(command) = self.commands.pop_front() {
                    while !self.execute_command(&command) {
                        warn!("Failed to execute command. Retrying...");
                    }
                } else {
                    debug!("Nothing to process. Waiting for command...");
                }
            }
            State::Dkg | State::Sign => {
                // We cannot execute the next command until the current one is finished...
                // Do nothing...
                debug!("Waiting for operation to finish");
            }
        }
    }

    /// Process the event as both a signer and a coordinator
    fn process_event(
        &mut self,
        event: &StackerDBChunksEvent,
    ) -> (Vec<Packet>, Vec<OperationResult>) {
        // Determine the current coordinator id and public key for verification
        let (coordinator_id, coordinator_public_key) =
            calculate_coordinator(&self.signing_round.public_keys);
        // Filter out invalid messages
        let inbound_messages: Vec<Packet> = event
            .modified_slots
            .iter()
            .filter_map(|chunk| {
                let message = bincode::deserialize::<Packet>(&chunk.data).ok()?;
                if verify_msg(
                    &message,
                    &self.signing_round.public_keys,
                    coordinator_public_key,
                ) {
                    Some(message)
                } else {
                    None
                }
            })
            .collect();
        // First process all messages as a signer
        let mut outbound_messages = self
            .signing_round
            .process_inbound_messages(&inbound_messages)
            .unwrap_or_default();
        // If the signer is the coordinator, then next process the message as the coordinator
        let (messages, results) = if self.signing_round.signer_id == coordinator_id {
            self.coordinator
                .process_inbound_messages(&inbound_messages)
                .unwrap_or_default()
        } else {
            (vec![], vec![])
        };
        outbound_messages.extend(messages);
        (outbound_messages, results)
    }
}

impl From<&Config> for RunLoop<FrostCoordinator<v2::Aggregator>> {
    /// Creates new runloop from a config
    fn from(config: &Config) -> Self {
        // TODO: this should be a config option
        // See: https://github.com/stacks-network/stacks-blockchain/issues/3914
        let threshold = ((config.signer_ids_public_keys.key_ids.len() * 7) / 10)
            .try_into()
            .unwrap();
        let total_signers = config
            .signer_ids_public_keys
            .signers
            .len()
            .try_into()
            .unwrap();
        let total_keys = config
            .signer_ids_public_keys
            .key_ids
            .len()
            .try_into()
            .unwrap();
        let key_ids = config
            .signer_key_ids
            .get(&config.signer_id)
            .unwrap()
            .iter()
            .map(|i| i - 1) // SigningRound::new (unlike SigningRound::from) doesn't do this
            .collect::<Vec<u32>>();
        let mut coordinator = FrostCoordinator::new(
            total_signers,
            total_keys,
            threshold,
            config.message_private_key,
        );
        let signing_round = SigningRound::new(
            threshold,
            total_signers,
            total_keys,
            config.signer_id,
            key_ids,
            config.message_private_key,
            config.signer_ids_public_keys.clone(),
        );
        let stacks_client = StacksClient::from(config);
        // Load the aggregate public key from the stacks client if it is set
        match stacks_client.get_aggregate_public_key() {
            Ok(key) => coordinator.set_aggregate_public_key(key),
            Err(e) => {
                // TODO: is this a fatal error? If we fail at startup to access the stacks client to see if DKG was set...this seems pretty fatal..
                panic!(
                    "Failed to load aggregate public key from stacks client: {:?}",
                    e
                );
            }
        }
        RunLoop {
            event_timeout: config.event_timeout,
            coordinator,
            signing_round,
            stacks_client,
            commands: VecDeque::new(),
            state: State::Idle,
        }
    }
}

impl<C: Coordinatable> SignerRunLoop<Vec<OperationResult>, RunLoopCommand> for RunLoop<C> {
    fn set_event_timeout(&mut self, timeout: Duration) {
        self.event_timeout = timeout;
    }

    fn get_event_timeout(&self) -> Duration {
        self.event_timeout
    }

    fn run_one_pass(
        &mut self,
        event: Option<StackerDBChunksEvent>,
        cmd: Option<RunLoopCommand>,
        res: Sender<Vec<OperationResult>>,
    ) -> Option<Vec<OperationResult>> {
        if let Some(command) = cmd {
            self.commands.push_back(command);
        }
        // First process any arrived events
        if let Some(event) = event {
            let (outbound_messages, operation_results) = self.process_event(&event);
            debug!(
                "Sending {} messages to other stacker-db instances.",
                outbound_messages.len()
            );
            for msg in outbound_messages {
                let ack = self
                    .stacks_client
                    .send_message(self.signing_round.signer_id, msg);
                if let Ok(ack) = ack {
                    debug!("ACK: {:?}", ack);
                } else {
                    warn!("Failed to send message to stacker-db instance: {:?}", ack);
                }
            }

            let nmb_results = operation_results.len();
            if nmb_results > 0 {
                // We finished our command. Update the state
                self.state = State::Idle;
                match res.send(operation_results) {
                    Ok(_) => debug!("Successfully sent {} operation result(s)", nmb_results),
                    Err(e) => {
                        warn!("Failed to send operation results: {:?}", e);
                    }
                }
            }
        }

        // Determine if we need to run DKG round
        if self.should_run_dkg() {
            // Add it to the front of the queue so it is set before any other commands get processed!
            debug!("DKG has not run and needs to be queued. Adding it to the front of the queue.");
            self.commands.push_back(RunLoopCommand::Dkg);
        }
        // The process the next command
        // Must be called AFTER processing the event as the state may update to IDLE due to said event.
        self.process_next_command();
        None
    }
}

/// Helper function for determining the coordinator public key given the the public keys
fn calculate_coordinator(public_keys: &PublicKeys) -> (u32, &ecdsa::PublicKey) {
    // TODO: do some sort of VRF here to calculate the public key
    // See: https://github.com/stacks-network/stacks-blockchain/issues/3915
    // Mockamato just uses the first signer_id as the coordinator for now
    (0, public_keys.signers.get(&0).unwrap())
}

/// TODO: this should not be here.
/// Temporary copy paste from frost-signer
/// See: https://github.com/stacks-network/stacks-blockchain/issues/3913
fn verify_msg(
    m: &Packet,
    public_keys: &PublicKeys,
    coordinator_public_key: &ecdsa::PublicKey,
) -> bool {
    match &m.msg {
        Message::DkgBegin(msg) | Message::DkgPrivateBegin(msg) => {
            if !msg.verify(&m.sig, coordinator_public_key) {
                warn!("Received a DkgPrivateBegin message with an invalid signature.");
                return false;
            }
        }
        Message::DkgEnd(msg) => {
            if let Some(public_key) = public_keys.signers.get(&msg.signer_id) {
                if !msg.verify(&m.sig, public_key) {
                    warn!("Received a DkgPublicEnd message with an invalid signature.");
                    return false;
                }
            } else {
                warn!(
                    "Received a DkgPublicEnd message with an unknown id: {}",
                    msg.signer_id
                );
                return false;
            }
        }
        Message::DkgPublicShares(msg) => {
            if let Some(public_key) = public_keys.signers.get(&msg.signer_id) {
                if !msg.verify(&m.sig, public_key) {
                    warn!("Received a DkgPublicShares message with an invalid signature.");
                    return false;
                }
            } else {
                warn!(
                    "Received a DkgPublicShares message with an unknown id: {}",
                    msg.signer_id
                );
                return false;
            }
        }
        Message::DkgPrivateShares(msg) => {
            // Private shares have key IDs from [0, N) to reference IDs from [1, N]
            // in Frost V4 to enable easy indexing hence ID + 1
            // TODO: Once Frost V5 is released, this off by one adjustment will no longer be required
            if let Some(public_key) = public_keys.signers.get(&msg.signer_id) {
                if !msg.verify(&m.sig, public_key) {
                    warn!("Received a DkgPrivateShares message with an invalid signature from signer_id {} key {}", msg.signer_id, &public_key);
                    return false;
                }
            } else {
                warn!(
                    "Received a DkgPrivateShares message with an unknown id: {}",
                    msg.signer_id
                );
                return false;
            }
        }
        Message::NonceRequest(msg) => {
            if !msg.verify(&m.sig, coordinator_public_key) {
                warn!("Received a NonceRequest message with an invalid signature.");
                return false;
            }
        }
        Message::NonceResponse(msg) => {
            if let Some(public_key) = public_keys.signers.get(&msg.signer_id) {
                if !msg.verify(&m.sig, public_key) {
                    warn!("Received a NonceResponse message with an invalid signature.");
                    return false;
                }
            } else {
                warn!(
                    "Received a NonceResponse message with an unknown id: {}",
                    msg.signer_id
                );
                return false;
            }
        }
        Message::SignatureShareRequest(msg) => {
            if !msg.verify(&m.sig, coordinator_public_key) {
                warn!("Received a SignatureShareRequest message with an invalid signature.");
                return false;
            }
        }
        Message::SignatureShareResponse(msg) => {
            if let Some(public_key) = public_keys.signers.get(&msg.signer_id) {
                if !msg.verify(&m.sig, public_key) {
                    warn!("Received a SignatureShareResponse message with an invalid signature.");
                    return false;
                }
            } else {
                warn!(
                    "Received a SignatureShareResponse message with an unknown id: {}",
                    msg.signer_id
                );
                return false;
            }
        }
    }
    true
}
