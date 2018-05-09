//! Binary Byzantine agreement protocol from a common coin protocol.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::hash::Hash;

use proto::message;

/// Type of output from the Agreement message handler. The first component is
/// the value on which the Agreement has decided, also called "output" in the
/// HoneyadgerBFT paper. The second component is a queue of messages to be sent
/// to remote nodes as a result of handling the incomming message.
type AgreementOutput = (Option<bool>, VecDeque<AgreementMessage>);

/// Messages sent during the binary Byzantine agreement stage.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum AgreementMessage {
    /// BVAL message with an epoch.
    BVal((u32, bool)),
    /// AUX message with an epoch.
    Aux((u32, bool)),
}

impl AgreementMessage {
    pub fn into_proto(self) -> message::AgreementProto {
        let mut p = message::AgreementProto::new();
        match self {
            AgreementMessage::BVal((e, b)) => {
                p.set_epoch(e);
                p.set_bval(b);
            }
            AgreementMessage::Aux((e, b)) => {
                p.set_epoch(e);
                p.set_aux(b);
            }
        }
        p
    }

    // TODO: Re-enable lint once implemented.
    #[cfg_attr(feature = "cargo-clippy", allow(needless_pass_by_value))]
    pub fn from_proto(mp: message::AgreementProto) -> Option<Self> {
        let epoch = mp.get_epoch();
        if mp.has_bval() {
            Some(AgreementMessage::BVal((epoch, mp.get_bval())))
        } else if mp.has_aux() {
            Some(AgreementMessage::Aux((epoch, mp.get_aux())))
        } else {
            None
        }
    }
}

pub struct Agreement<NodeUid> {
    /// The UID of the corresponding proposer node.
    uid: NodeUid,
    num_nodes: usize,
    num_faulty_nodes: usize,
    epoch: u32,
    /// Bin values. Reset on every epoch update.
    bin_values: BTreeSet<bool>,
    /// Values received in BVAL messages. Reset on every epoch update.
    received_bval: HashMap<NodeUid, BTreeSet<bool>>,
    /// Sent BVAL values. Reset on every epoch update.
    sent_bval: BTreeSet<bool>,
    /// Values received in AUX messages. Reset on every epoch update.
    received_aux: HashMap<NodeUid, bool>,
    /// Estimates of the decision value in all epochs. The first estimated value
    /// is provided as input by Common Subset using the `set_input` function
    /// which triggers the algorithm to start.
    estimated: BTreeMap<u32, bool>,
    /// Termination flag. The Agreement instance doesn't terminate immediately
    /// upon deciding on the agreed value. This is done in order to help other
    /// nodes decide despite asynchrony of communication. Once the instance
    /// determines that all the remote nodes have reached agreement, it sets the
    /// `terminated` flag and accepts no more incoming messages.
    terminated: bool,
}

impl<NodeUid: Clone + Eq + Hash> Agreement<NodeUid> {
    pub fn new(uid: NodeUid, num_nodes: usize) -> Self {
        let num_faulty_nodes = (num_nodes - 1) / 3;

        Agreement {
            uid,
            num_nodes,
            num_faulty_nodes,
            epoch: 0,
            bin_values: BTreeSet::new(),
            received_bval: HashMap::new(),
            sent_bval: BTreeSet::new(),
            received_aux: HashMap::new(),
            estimated: BTreeMap::new(),
            terminated: false,
        }
    }

    /// Algorithm has terminated.
    pub fn terminated(&self) -> bool {
        self.terminated
    }

    pub fn set_input(&mut self, input: bool) -> Result<AgreementMessage, Error> {
        if self.epoch != 0 {
            return Err(Error::InputNotAccepted);
        }

        // Set the initial estimated value to the input value.
        self.estimated.insert(self.epoch, input);
        // Receive the BVAL message locally.
        self.received_bval
            .entry(self.uid.clone())
            .or_insert_with(BTreeSet::new)
            .insert(input);
        // Multicast BVAL
        Ok(AgreementMessage::BVal((self.epoch, input)))
    }

    pub fn has_input(&self) -> bool {
        self.estimated.get(&0).is_some()
    }

    /// Receive input from a remote node.
    ///
    /// Outputs an optional agreement result and a queue of agreement messages
    /// to remote nodes. There can be up to 2 messages.
    pub fn handle_agreement_message(
        &mut self,
        sender_id: NodeUid,
        message: &AgreementMessage,
    ) -> Result<AgreementOutput, Error> {
        match *message {
            // The algorithm instance has already terminated.
            _ if self.terminated => Err(Error::Terminated),

            AgreementMessage::BVal((epoch, b)) if epoch == self.epoch => {
                self.handle_bval(sender_id, b)
            }

            AgreementMessage::Aux((epoch, b)) if epoch == self.epoch => {
                self.handle_aux(sender_id, b)
            }

            // Epoch does not match. Ignore the message.
            _ => Ok((None, VecDeque::new())),
        }
    }

    fn handle_bval(&mut self, sender_id: NodeUid, b: bool) -> Result<AgreementOutput, Error> {
        let mut outgoing = VecDeque::new();

        self.received_bval
            .entry(sender_id)
            .or_insert_with(BTreeSet::new)
            .insert(b);
        let count_bval = self.received_bval
            .values()
            .filter(|values| values.contains(&b))
            .count();

        // upon receiving BVAL_r(b) messages from 2f + 1 nodes,
        // bin_values_r := bin_values_r ∪ {b}
        if count_bval == 2 * self.num_faulty_nodes + 1 {
            self.bin_values.insert(b);

            // wait until bin_values_r != 0, then multicast AUX_r(w)
            // where w ∈ bin_values_r
            if self.bin_values.len() == 1 {
                // Send an AUX message at most once per epoch.
                outgoing.push_back(AgreementMessage::Aux((self.epoch, b)));
                // Receive the AUX message locally.
                self.received_aux.insert(self.uid.clone(), b);
            }

            let coin_result = self.try_coin();
            Ok((coin_result, outgoing))
        }
        // upon receiving BVAL_r(b) messages from f + 1 nodes, if
        // BVAL_r(b) has not been sent, multicast BVAL_r(b)
        else if count_bval == self.num_faulty_nodes + 1 && !self.sent_bval.contains(&b) {
            outgoing.push_back(AgreementMessage::BVal((self.epoch, b)));
            // Receive the BVAL message locally.
            self.received_bval
                .entry(self.uid.clone())
                .or_insert_with(BTreeSet::new)
                .insert(b);
            Ok((None, outgoing))
        } else {
            Ok((None, outgoing))
        }
    }

    fn handle_aux(&mut self, sender_id: NodeUid, b: bool) -> Result<AgreementOutput, Error> {
        self.received_aux.insert(sender_id, b);
        if !self.bin_values.is_empty() {
            let coin_result = self.try_coin();
            Ok((coin_result, VecDeque::new()))
        } else {
            Ok((None, VecDeque::new()))
        }
    }

    /// AUX_r messages such that the set of values carried by those messages is
    /// a subset of bin_values_r. Outputs this subset.
    ///
    /// FIXME: Clarify whether the values of AUX messages should be the same or
    /// not. It is assumed in `count_aux` that they can differ.
    ///
    /// In general, we can't expect every good node to send the same AUX value,
    /// so waiting for N - f agreeing messages would not always terminate. We
    /// can, however, expect every good node to send an AUX value that will
    /// eventually end up in our bin_values.
    fn count_aux(&self) -> (usize, BTreeSet<bool>) {
        let mut vals: BTreeSet<bool> = BTreeSet::new();

        for b in self.received_aux.values() {
            if self.bin_values.contains(b) {
                vals.insert(b.clone());
            }
        }

        (vals.len(), vals)
    }

    /// Waits until at least (N − f) AUX_r messages have been received, such that
    /// the set of values carried by these messages, vals, are a subset of
    /// bin_values_r (note that bin_values_r may continue to change as BVAL_r
    /// messages are received, thus this condition may be triggered upon arrival
    /// of either an AUX_r or a BVAL_r message).
    ///
    /// `try_coin` outputs an optional decision value of the agreement instance.
    fn try_coin(&mut self) -> Option<bool> {
        let (count_aux, vals) = self.count_aux();
        if count_aux < self.num_nodes - self.num_faulty_nodes {
            // Continue waiting for the (N - f) AUX messages.
            return None;
        }

        // FIXME: Implement the Common Coin algorithm. At the moment the
        // coin value is common across different nodes but not random.
        let coin2 = (self.epoch % 2) == 0;

        // Check the termination condition: "continue looping until both a
        // value b is output in some round r, and the value Coin_r' = b for
        // some round r' > r."
        self.terminated = self.terminated || self.estimated.values().any(|b| *b == coin2);

        // Start the next epoch.
        self.bin_values.clear();
        self.received_aux.clear();
        self.epoch += 1;

        if vals.len() != 1 {
            self.estimated.insert(self.epoch, coin2);
            return None;
        }

        // NOTE: `vals` has exactly one element due to `vals.len() == 1`
        let output: Vec<Option<bool>> = vals.into_iter()
            .take(1)
            .map(|b| {
                self.estimated.insert(self.epoch, b);
                if b == coin2 {
                    // Output the agreement value.
                    Some(b)
                } else {
                    // Don't output a value.
                    None
                }
            })
            .collect();

        output[0]
    }
}

#[derive(Clone, Debug)]
pub enum Error {
    Terminated,
    InputNotAccepted,
}
