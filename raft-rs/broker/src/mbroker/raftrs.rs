// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

// We use `default` method a lot to be support prost and rust-protobuf at the
// same time. And reassignment can be optimized by compiler.
#![allow(clippy::field_reassign_with_default)]

use nix::sys::socket::{setsockopt, sockopt};
use slog::{debug, Drain};
use std::collections::{BTreeMap, VecDeque};
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
// use std::ffi::NulError;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use std::thread;

use protobuf::Message as PbMessage;
use raft::storage::MemStorage;
use raft::{prelude::*, StateRole};
// use regex::Regex;

use slog::{error, o};

use super::is_ready_to_send;
use super::message::Message as MbMessage;
use super::message::MessageType as MbMessageType;
use super::queue::{MQueue, MQueuePool};

const LEADER_NODE: u64 = 6555;

pub fn start_raft(proposals: Arc<Mutex<VecDeque<Proposal>>>, mq_pool: Arc<RwLock<MQueuePool>>, raft_nodes: u32, my_address: String) {
    let decorator = slog_term::TermDecorator::new().build();
    let drain = slog_term::FullFormat::new(decorator).build().fuse();
    let drain = slog_async::Async::new(drain)
        .chan_size(4096)
        .overflow_strategy(slog_async::OverflowStrategy::Block)
        .build()
        .fuse();
    let logger = slog::Logger::root(drain, o!());

    let num_nodes: u64 = raft_nodes as u64;

    // Channels for getting streams from ather temporary threads
    let (accept_tx, accept_rx) = mpsc::channel();
    let (connect_tx, connect_rx) = mpsc::channel();

    // let my_address_clone = my_address.clone();
    let addr = my_address.clone().split(":").collect::<Vec<&str>>()[0].to_string();
    let my_port = my_address.split(":").collect::<Vec<&str>>()[1].to_string();
    let mut my_port = my_port.parse::<u64>().unwrap();
    // ブローカの port 番号から 1000 を加算して Raft 用の port 番号を設定
    my_port += 1000;
    let addr_clone = addr.clone();
    let my_port_clone = my_port;

    // Get streams for recv from other nodes
    thread::spawn(move || {
        let listener = TcpListener::bind(format!("{}:{}", addr_clone, my_port_clone)).unwrap();
        setsockopt(&listener, sockopt::ReuseAddr, &true).unwrap();

        let mut streams: BTreeMap<u64, TcpStream> = BTreeMap::new();
        for _ in 1..num_nodes {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_nodelay(true).unwrap();

            let mut size_buf = [0; 4];
            stream.read_exact(&mut size_buf).unwrap();
            let size = u32::from_be_bytes(size_buf) as usize;
            let mut buf = vec![0; size];
            stream.read_exact(&mut buf).unwrap();

            let port = u64::from_be_bytes(buf.try_into().unwrap());
            stream.set_nonblocking(true).unwrap();
            streams.insert(port, stream);
        }
        // return streams to main thread
        accept_tx.send(streams).unwrap();
    });

    // Get streams for send to other nodes
    thread::spawn(move || {
        let mut streams: BTreeMap<u64, TcpStream> = BTreeMap::new();

        // FIXME: ブローカの port 番号が5555以降の連番であることに依存している
        for port in LEADER_NODE..LEADER_NODE+num_nodes {
            if port == my_port_clone {
                continue; // skip if port is my port
            }
            loop {
                match TcpStream::connect(format!("{}:{}", addr, port)) {
                    Ok(mut stream) => {
                        stream.set_nodelay(true).unwrap();

                        let bytes = my_port_clone.to_be_bytes();
                        let size = bytes.len();
                        stream.write_all(&(size as u32).to_be_bytes()).unwrap();
                        stream.write_all(&bytes).unwrap();

                        streams.insert(port, stream);
                        break;
                    }
                    Err(_) => {
                        thread::sleep(Duration::from_millis(100));
                        continue;
                    }
                }
            }
        }
        // return streams to main thread
        connect_tx.send(streams).unwrap();
    });

    // Get streams from temporary threads
    // recv_streams is for receiving messages from other nodes.
    // send_streams is for sending messages to other nodes.
    let recv_streams = accept_rx.recv().unwrap();
    let send_streams = connect_rx.recv().unwrap();

    // recv_streams と send_stream から，BtreeMap (port - LEADER_NODE +1, (send_stream, recv_stream)) を作成
    let streams: BTreeMap<u64, (TcpStream, TcpStream)> = recv_streams.iter()
                                                                    .map(
                                                                        |(k, v)| (*k - LEADER_NODE +1, (send_streams.get(k).unwrap().try_clone().unwrap(), v.try_clone().unwrap()))
                                                                    )
                                                                    .collect();

    let mq_pool = Arc::clone(&mq_pool);
    let node = match my_port {
        // Peer 1 is the leader.
        LEADER_NODE => Node::create_raft_leader(1, streams, &logger, mq_pool),
        // Other peers are followers.
        _ => Node::create_raft_follower(streams),
    };

    // A global pending proposals queue. New proposals will be pushed back into the queue, and
    // after it's committed by the raft cluster, it will be poped from the queue.
    let proposals_clone = Arc::clone(&proposals);
    let logger = logger.clone();
    // Here we spawn the node on a new thread and keep a handle so we can join on them later.
    let handle = thread::spawn(move || run_node(node, proposals_clone,  logger));

    // Propose some conf changes so that followers can be initialized.
    let proposals = Arc::clone(&proposals);
    if my_port == LEADER_NODE {
        add_all_followers(proposals.as_ref(), num_nodes);
    }

    // Wait for the thread to finish
    // No return because the broker uses Raft
    handle.join().unwrap();
}

fn run_node(
    mut node: Node,
    proposals: Arc<Mutex<VecDeque<Proposal>>>,
    logger: slog::Logger,
){
    // Channels for receiving messages from other nodes.
    let (recv_tx, recv_rx) = mpsc::channel();
    let mut send_txs: BTreeMap<u64, Sender<Vec<Message>>> = BTreeMap::new();
    if ! node.streams.is_empty() {
        let keys: Vec<u64> = node.streams.keys().cloned().collect();
        for key in keys {
            // Channels for sending messages to other nodes.
            let (send_tx, send_rx) = mpsc::channel();
            send_txs.insert(key, send_tx);
            let recv_tx = recv_tx.clone();
            let mut stream_pair = node.streams.remove(&key).unwrap();
            let logger = logger.clone();
            thread::spawn(move || {
                treat_node(&mut stream_pair.0, &mut stream_pair.1, send_rx, recv_tx, logger);
            });
        }
    }

    // Tick the raft node per 100ms. So use an `Instant` to trace it.
    let mut t = Instant::now();
    loop {
        loop {
            match recv_rx.try_recv() {
                Ok(msg) => node.step(msg, &logger),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }

        let raft_group = match node.raft_group {
            Some(ref mut r) => r,
            // When Node::raft_group is `None` it means the node is not initialized.
            _ => continue,
        };

        if t.elapsed() >= Duration::from_millis(100) {
            // Tick the raft.
            raft_group.tick();
            t = Instant::now();
        }

        // Let the leader pick pending proposals from the global queue.
        if raft_group.raft.state == StateRole::Leader {
            // Handle new proposals.
            let mut proposals = proposals.lock().unwrap();
            for p in proposals.iter_mut().skip_while(|p| p.proposed > 0) {
                propose(raft_group, p);
            }
        }

        // Handle readies from the raft.
        on_ready(
            raft_group,
            Arc::clone(&node.mq_pool),
            &mut send_txs,
            &proposals,
            &logger
        );
    }
}

fn treat_node(
    send_stream: &mut TcpStream,
    recv_stream: &mut TcpStream,
    send_rx: Receiver<Vec<Message>>,
    recv_tx: Sender<Message>,
    logger: slog::Logger,
) {
    loop {
        // If there are messages to send to other nodes, send it.
        match send_rx.try_recv() {
            Ok(msgs) => {
                for msg in msgs {
                    let to = msg.to;
                    let bytes = msg.write_to_bytes().unwrap();
                    let size = bytes.len();
                    send_stream.write_all(&size.to_be_bytes()).unwrap();
                    if send_stream.write_all(&bytes).is_err() {
                        error!(
                            logger,
                            "send raft message to {} fail, let Raft retry it", to
                        );
                    }
                }
            },
            Err(TryRecvError::Empty) => (),
            Err(TryRecvError::Disconnected) => {
                error!(logger, "send_rx disconnected");
                return;
            },
        }

        // If there are messages received from other nodes, send it to the Raft node.
        let mut size_buf = [0; 4];
        match recv_stream.read_exact(&mut size_buf) {
            Ok(_) => {
                let size = u32::from_be_bytes(size_buf) as usize;
                let mut buf = vec![0; size];
                loop {
                    match recv_stream.read_exact(&mut buf) {
                        Ok(_) => {
                            let msg = Message::parse_from_bytes(&buf);
                            if let Ok(msg) = msg {
                                debug!(logger, "{:?}", msg);
                                let _ = recv_tx.send(msg);
                            }
                            break;
                        }
                        Err(e) => {
                            match e.kind() {
                                ErrorKind::WouldBlock => (),
                                _ => break,
                            }
                        }
                    }
                }
            }
            Err(e) => {
                match e.kind() {
                    ErrorKind::WouldBlock => (),
                    _ => break
                }
            }
        }
    }
}

#[allow(dead_code)]
enum Signal {
    Terminate,
}

#[allow(dead_code)]
fn check_signals(receiver: &Arc<Mutex<mpsc::Receiver<Signal>>>) -> bool {
    match receiver.lock().unwrap().try_recv() {
        Ok(Signal::Terminate) => true,
        Err(TryRecvError::Empty) => false,
        Err(TryRecvError::Disconnected) => true,
        // _ => false,
    }
}

struct Node {
    // None if the raft is not initialized.
    raft_group: Option<RawNode<MemStorage>>,
    streams: BTreeMap<u64, (TcpStream, TcpStream)>,
    // Key-value pairs after applied. `MemStorage` only contains raft logs,
    // so we need an additional storage engine.
    mq_pool: Arc<RwLock<MQueuePool>>,
}

impl Node {
    // Create a raft leader only with itself in its configuration.
    fn create_raft_leader(
        id: u64,
        streams: BTreeMap<u64, (TcpStream, TcpStream)>,
        logger: &slog::Logger,
        mq_pool: Arc<RwLock<MQueuePool>>,
    ) -> Self {
        let mut cfg = example_config();
        cfg.id = id;
        let logger = logger.new(o!("tag" => format!("peer_{}", id)));
        let mut s = Snapshot::default();
        // Because we don't use the same configuration to initialize every node, so we use
        // a non-zero index to force new followers catch up logs by snapshot first, which will
        // bring all nodes to the same initial state.
        s.mut_metadata().index = 1;
        s.mut_metadata().term = 1;
        s.mut_metadata().mut_conf_state().voters = vec![1];
        let storage = MemStorage::new();
        storage.wl().apply_snapshot(s).unwrap();
        let raft_group = Some(RawNode::new(&cfg, storage, &logger).unwrap());
        Node {
            raft_group,
            streams,
            mq_pool,
        }
    }

    // Create a raft follower.
    fn create_raft_follower(
        streams: BTreeMap<u64, (TcpStream, TcpStream)>,
    ) -> Self {
        Node {
            raft_group: None,
            streams,
            mq_pool: Arc::new(RwLock::new(MQueuePool::new())),
        }
    }

    // Initialize raft for followers.
    fn initialize_raft_from_message(&mut self, msg: &Message, logger: &slog::Logger) {
        if !is_initial_msg(msg) {
            return;
        }
        let mut cfg = example_config();
        cfg.id = msg.to;
        let logger = logger.new(o!("tag" => format!("peer_{}", msg.to)));
        let storage = MemStorage::new();
        self.raft_group = Some(RawNode::new(&cfg, storage, &logger).unwrap());
    }

    // Step a raft message, initialize the raft if need.
    fn step(&mut self, msg: Message, logger: &slog::Logger) {
        if self.raft_group.is_none() {
            if is_initial_msg(&msg) {
                self.initialize_raft_from_message(&msg, logger);
            } else {
                return;
            }
        }
        let raft_group = self.raft_group.as_mut().unwrap();
        let _ = raft_group.step(msg);
    }
}

fn on_ready(
    raft_group: &mut RawNode<MemStorage>,
    mq_pool: Arc<RwLock<MQueuePool>>,
    send_txs: &mut BTreeMap<u64, Sender<Vec<Message>>>,
    proposals: &Mutex<VecDeque<Proposal>>,
    logger: &slog::Logger,
) {
    if !raft_group.has_ready() {
        return;
    }
    let store = raft_group.raft.raft_log.store.clone();

    // Get the `Ready` with `RawNode::ready` interface.
    let mut ready = raft_group.ready();

    fn handle_messages(msgs: Vec<Message>, send_txs: &mut BTreeMap<u64, Sender<Vec<Message>>>) {
        let mut msgs_group: BTreeMap<u64, Vec<Message>> = BTreeMap::new();
        for msg in msgs {
            let key = msg.to;
            // devide the messages by the destination node.
            msgs_group.entry(key).or_default().push(msg);
        }
        for (key, send_tx) in send_txs {
            if let Some(value) = msgs_group.remove(key) {
                send_tx.send(value).unwrap();
            }
        }
    }

    if !ready.messages().is_empty() {
        // Send out the messages come from the node.
        handle_messages(ready.take_messages(), send_txs);
    }

    // Apply the snapshot. It's necessary because in `RawNode::advance` we stabilize the snapshot.
    if *ready.snapshot() != Snapshot::default() {
        let s = ready.snapshot().clone();
        if let Err(e) = store.wl().apply_snapshot(s) {
            error!(
                logger,
                "apply snapshot fail: {:?}, need to retry or panic", e
            );
            return;
        }
    }

    let handle_committed_entries =
        |rn: &mut RawNode<MemStorage>, committed_entries: Vec<Entry>| {
            for entry in committed_entries {
                if entry.data.is_empty() {
                    // From new elected leaders.
                    continue;
                }
                let res = if let EntryType::EntryConfChange = entry.get_entry_type() {
                    // For conf change messages, make them effective.
                    let mut cc = ConfChange::default();
                    cc.merge_from_bytes(&entry.data).unwrap();
                    let cs = rn.apply_conf_change(&cc).unwrap();
                    store.wl().set_conf_state(cs);
                    None
                } else {
                    // For normal proposals, extract the key-value pair and then
                    // insert them into the kv engine.
                    let msg = MbMessage::from_bytes(&entry.data);

                    let res = match msg.header.msg_type() {
                        MbMessageType::SendReq => {
                            let mut mq_pool = mq_pool.write().unwrap();
                            let mqueue = match mq_pool.find_by_id(msg.header.daddr) {
                                Some(mqueue) => Arc::clone(mqueue),
                                None => {
                                    let client_id = msg.header.daddr;
                                    Arc::clone(mq_pool.add(client_id, MQueue::new(client_id)))
                                }
                            };
                            drop(mq_pool);
                            debug!(logger, "peer {}: process SendReq: {:?}", rn.raft.id, msg.header.id);
                            mqueue.write().unwrap().waiting_queue.enqueue(msg);
                            None
                        }
                        MbMessageType::FreeReq => {
                            let msg_id = msg.header.id;
                            let saddr = msg.header.saddr;
                            let mqueue = {
                                let mq_pool = mq_pool.read().unwrap();
                                mq_pool.find_by_id(saddr).unwrap().clone()
                            };
                            mqueue
                                .write()
                                .unwrap()
                                .delivered_queue
                                .dequeue_by(|queued_msg| queued_msg.header.id == msg_id)
                                .unwrap();
                            debug!(logger, "peer {}: process FreeReq: {:?}", rn.raft.id, msg_id);
                            None
                        }
                        MbMessageType::PushReq => {
                            let saddr = msg.header.saddr;
                            let mqueue = {
                                let mq_pool = mq_pool.read().unwrap();
                                mq_pool.find_by_id(saddr).unwrap().clone()
                            };
                            let mut mqueue = mqueue.write().unwrap();
                            if is_ready_to_send(&mqueue) {
                                let mut msg = mqueue.waiting_queue.dequeue().unwrap();
                                msg.header.change_msg_type(MbMessageType::PushReq);
                                // timer.append(msg.header.id, msg.header.msg_type(), time_now());
                                // stream.send_msg(&mut msg).unwrap();
                                mqueue.delivered_queue.enqueue(msg.clone());
                                debug!(logger, "peer {}: process inner PushReq or Timeout: {:?}", rn.raft.id, msg.header.id);
                                Some(msg)
                            } else {
                                None
                            }
                        }
                        MbMessageType::HeloReq => {
                            let client_id = msg.header.saddr;
                            {
                                let mut mq_pool = mq_pool.write().unwrap();
                                if mq_pool.find_by_id(client_id).is_none() {
                                    mq_pool.add(client_id, MQueue::new(client_id));
                                };
                            }
                            debug!(logger, "peer {}: process HeloReq: {:?}", rn.raft.id, msg.header.id);
                            None
                        }
                        _ => {
                            // The other MessageType will never be received
                            None
                        }
                    };
                    res
                };
                if rn.raft.state == StateRole::Leader {
                    // The leader should response to the clients, tell them if their proposals
                    // succeeded or not.
                    let proposal = proposals.lock().unwrap().pop_front().unwrap();
                    proposal.propose_success.send(res).unwrap();
                }
            }
        };
    // Apply all committed entries.
    handle_committed_entries(raft_group, ready.take_committed_entries());

    // Persistent raft logs. It's necessary because in `RawNode::advance` we stabilize
    // raft logs to the latest position.
    if let Err(e) = store.wl().append(ready.entries()) {
        error!(
            logger,
            "persist raft log fail: {:?}, need to retry or panic", e
        );
        return;
    }

    if let Some(hs) = ready.hs() {
        // Raft HardState changed, and we need to persist it.
        store.wl().set_hardstate(hs.clone());
    }

    if !ready.persisted_messages().is_empty() {
        // Send out the persisted messages come from the node.
        handle_messages(ready.take_persisted_messages(), send_txs);
    }

    // Call `RawNode::advance` interface to update position flags in the raft.
    let mut light_rd = raft_group.advance(ready);
    // Update commit index.
    if let Some(commit) = light_rd.commit_index() {
        store.wl().mut_hard_state().set_commit(commit);
    }
    // Send out the messages.
    handle_messages(light_rd.take_messages(), send_txs);
    // Apply all committed entries.
    handle_committed_entries(raft_group, light_rd.take_committed_entries());
    // Advance the apply index.
    raft_group.advance_apply();
}

fn example_config() -> Config {
    Config {
        election_tick: 10,
        heartbeat_tick: 3,
        ..Default::default()
    }
}

// The message can be used to initialize a raft node or not.
fn is_initial_msg(msg: &Message) -> bool {
    let msg_type = msg.get_msg_type();
    msg_type == MessageType::MsgRequestVote
        || msg_type == MessageType::MsgRequestPreVote
        || (msg_type == MessageType::MsgHeartbeat && msg.commit == 0)
}

#[derive(Clone)]
pub struct Proposal {
    normal: Option<MbMessage>, //log entry ?
    conf_change: Option<ConfChange>, // conf change.
    transfer_leader: Option<u64>,
    // If it's proposed, it will be set to the index of the entry.
    proposed: u64,
    propose_success: SyncSender<Option<MbMessage>>,
}

impl Proposal {
    fn conf_change(cc: &ConfChange) -> (Self, Receiver<Option<MbMessage>>) {
        let (tx, rx) = mpsc::sync_channel(1);
        let proposal = Proposal {
            normal: None,
            conf_change: Some(cc.clone()),
            transfer_leader: None,
            proposed: 0,
            propose_success: tx,
        };
        (proposal, rx)
    }

    pub fn normal(msg:MbMessage) -> (Self, Receiver<Option<MbMessage>>) {
        let (tx, rx) = mpsc::sync_channel(1);
        let proposal = Proposal {
            normal: Some(msg),
            conf_change: None,
            transfer_leader: None,
            proposed: 0,
            propose_success: tx,
        };
        (proposal, rx)
    }
}

fn propose(raft_group: &mut RawNode<MemStorage>, proposal: &mut Proposal) {
    let last_index1 = raft_group.raft.raft_log.last_index() + 1;
    {
        let proposal = proposal.clone();
        if let Some(mut msg) = proposal.normal {
            let data = msg.to_bytes();
            let _ = raft_group.propose(vec![], data.to_vec());
        } else if let Some(ref cc) = proposal.conf_change {
            let _ = raft_group.propose_conf_change(vec![], cc.clone());
        } else if let Some(_transferee) = proposal.transfer_leader {
            // TODO: implement transfer leader.
            unimplemented!();
        }
    }

    let last_index2 = raft_group.raft.raft_log.last_index() + 1;
    if last_index2 == last_index1 {
        // Propose failed, don't forget to respond to the client.
        proposal.propose_success.send(None).unwrap();
        // proposal.propose_success.send(false).unwrap();
    } else {
        proposal.proposed = last_index1;
    }
}

// Proposes some conf change for peers [2, 5].
fn add_all_followers(proposals: &Mutex<VecDeque<Proposal>>, num_nodes: u64) {
    for i in 2..num_nodes+1 {
        let mut conf_change = ConfChange::default();
        conf_change.node_id = i;
        conf_change.set_change_type(ConfChangeType::AddNode);
        loop {
            let (proposal, rx) = Proposal::conf_change(&conf_change);
            proposals.lock().unwrap().push_back(proposal);
            if rx.recv().unwrap().is_none() {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}
