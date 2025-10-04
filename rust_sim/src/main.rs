use rand::{RngCore, SeedableRng};
use rand_xoshiro::Xoroshiro128PlusPlus;
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::env;
use std::rc::{Rc, Weak};

// Faster hashmap
use ahash::AHashMap as HashMap;

// ---------------- Block ----------------

#[derive(Debug)]
struct Block {
    height: usize,
    time: usize,
    hash: [u8; 32],        // raw bytes, no hex string
    miner_id: Option<usize>,
    parent: Option<Weak<Block>>, // Weak to avoid deep recursive drop
    parent_hash: [u8; 32],       // cached for quick membership checks
}

impl Block {
    fn new(
        height: usize,
        time: usize,
        parent: Option<Rc<Block>>,
        miner_id: Option<usize>,
    ) -> Rc<Block> {
        let parent_hash = match parent.as_ref() {
            Some(p) => p.hash,
            None => [0u8; 32],
        };
        let parent_weak = parent.as_ref().map(Rc::downgrade);

        // Hash raw bytes instead of formatting strings
        let mut hasher = Sha256::new();
        hasher.update(height.to_le_bytes());
        hasher.update((miner_id.unwrap_or(usize::MAX) as u64).to_le_bytes());
        hasher.update(parent_hash);
        let hash: [u8; 32] = hasher.finalize().into();

        Rc::new(Block {
            height,
            time,
            hash,
            miner_id,
            parent: parent_weak,
            parent_hash,
        })
    }
}

// ---------------- Connection ----------------

struct QueueEntry {
    block: Rc<Block>,
    tts: usize, // time-to-send countdown
}

struct Connection {
    receiver: Weak<RefCell<Miner>>,
    delay: usize,
    blocks_to_send: Vec<QueueEntry>,
}

impl Connection {
    fn new(receiver: &Rc<RefCell<Miner>>, delay: usize) -> Self {
        Self {
            receiver: Rc::downgrade(receiver),
            delay,
            blocks_to_send: Vec::new(),
        }
    }

    fn queue_block(&mut self, block: Rc<Block>) {
        self.blocks_to_send.push(QueueEntry {
            block,
            tts: self.delay,
        });
    }

    fn send_block(&self, block: Rc<Block>) {
        if let Some(receiver_rc) = self.receiver.upgrade() {
            receiver_rc.borrow_mut().receive_block(block);
        }
    }

    // Borrow-checker safe and cache-friendly tick
    fn tick(&mut self) {
        if self.blocks_to_send.is_empty() {
            return;
        }

        // Take the queue to avoid aliasing, then split into send/keep
        let mut queue = std::mem::take(&mut self.blocks_to_send);

        let mut remaining = Vec::with_capacity(queue.len());
        let mut to_send: Vec<Rc<Block>> = Vec::new();

        for mut entry in queue.drain(..) {
            if entry.tts == 0 {
                to_send.push(entry.block);
            } else {
                entry.tts -= 1;
                remaining.push(entry);
            }
        }

        self.blocks_to_send = remaining;

        for block in to_send {
            self.send_block(block);
        }
    }
}

// ---------------- Miner ----------------

struct Miner {
    id: usize,
    name: String,
    hashrate_proportion: f64,
    current_block: Rc<Block>,
    block_candidates: Vec<Rc<Block>>,
    connections: Vec<Connection>,
    known_blocks: HashMap<[u8; 32], Rc<Block>>,
    rejected_blocks: HashMap<[u8; 32], Rc<Block>>,
    blocks_mined: usize,
    probability_per_second: f64,
    rng: Xoroshiro128PlusPlus, // fast per-miner RNG
}

impl Miner {
    fn new(id: usize, name: &str, initial_block: Rc<Block>, hashrate_proportion: f64) -> Self {
        let mut known_blocks: HashMap<[u8; 32], Rc<Block>> = HashMap::default();
        known_blocks.insert(initial_block.hash, initial_block.clone());
        let probability_per_second = hashrate_proportion * (1.0 - (-1.0f64 / 600.0).exp());

        Self {
            id,
            name: name.to_string(),
            hashrate_proportion,
            current_block: initial_block,
            block_candidates: Vec::new(),
            connections: Vec::new(),
            known_blocks,
            rejected_blocks: HashMap::default(),
            blocks_mined: 0,
            probability_per_second,
            rng: Xoroshiro128PlusPlus::from_entropy(),
        }
    }

    fn add_connection(&mut self, other: &Rc<RefCell<Miner>>, delay: usize) {
        self.connections.push(Connection::new(other, delay));
    }

    fn is_mine(&self, block: &Rc<Block>) -> bool {
        block.miner_id == Some(self.id)
    }

    // 53-bit precise uniform in [0,1)
    #[inline]
    fn rand_f64(&mut self) -> f64 {
        const SCALE: f64 = 1.0 / ((1u64 << 53) as f64);
        let x = self.rng.next_u64() >> 11;
        (x as f64) * SCALE
    }

    // Unbiased index in [0, n) using Lemire's method
    #[inline]
    fn rand_index(&mut self, n: usize) -> usize {
        debug_assert!(n > 0);
        let r = self.rng.next_u64();
        (((r as u128) * (n as u128)) >> 64) as usize
    }

    fn evaluate_candidates(&mut self) {
        let max_height = self.block_candidates.iter().map(|b| b.height).max().unwrap();

        let candidates_max_height: Vec<Rc<Block>> = self
            .block_candidates
            .iter()
            .filter(|b| b.height == max_height)
            .cloned()
            .collect();

        if let Some(own_block) = candidates_max_height.iter().find(|b| self.is_mine(b)) {
            self.current_block = own_block.clone();
            self.block_candidates.clear();
            return;
        }

        let idx = self.rand_index(candidates_max_height.len());
        self.current_block = candidates_max_height[idx].clone();
        self.block_candidates.clear();
    }

    fn mine(&mut self, time: usize) {
        if !self.block_candidates.is_empty() {
            self.evaluate_candidates();
        }

        if self.rand_f64() < self.probability_per_second {
            let found_block = Block::new(
                self.current_block.height + 1,
                time,
                Some(self.current_block.clone()),
                Some(self.id),
            );
            self.known_blocks.insert(found_block.hash, found_block.clone());
            self.block_candidates.push(found_block.clone());
            self.announce(found_block);
        }
    }

    fn announce(&mut self, block: Rc<Block>) {
        self.blocks_mined += 1;
        for connection in &mut self.connections {
            connection.queue_block(block.clone());
        }
    }

    fn send_messages(&mut self) {
        for connection in &mut self.connections {
            connection.tick();
        }
    }

    fn refresh_rejects(&mut self) {
        loop {
            let mut moved: Option<[u8; 32]> = None;

            for (reject_hash, reject_block) in self.rejected_blocks.iter() {
                if self.known_blocks.contains_key(&reject_block.parent_hash) {
                    moved = Some(*reject_hash);
                    break;
                }
            }

            if let Some(key) = moved {
                if let Some(block) = self.rejected_blocks.remove(&key) {
                    self.known_blocks.insert(key, block);
                }
            } else {
                break;
            }
        }
    }

    fn receive_block(&mut self, block: Rc<Block>) {
        // Only true for genesis, which we'll never receive.
        assert!(block.parent.is_some());

        if self.known_blocks.contains_key(&block.hash) {
            return;
        }

        if !self.known_blocks.contains_key(&block.parent_hash) {
            self.rejected_blocks.insert(block.hash, block);
            return;
        }

        self.known_blocks.insert(block.hash, block.clone());
        self.refresh_rejects();

        if block.height > self.current_block.height {
            self.block_candidates.push(block);
        }
    }
}

// ---------------- Main ----------------

fn main() {
    // Parse optional argument: block_periods (default 10000)
    let args: Vec<String> = env::args().collect();
    let block_periods_to_simulate: usize = if args.len() > 1 {
        args[1].parse().unwrap_or(10000)
    } else {
        10000
    };

    let seconds_to_simulate = block_periods_to_simulate * 600;

    let genesis_block = Block::new(0, 0, None, None);

    let miners: Vec<Rc<RefCell<Miner>>> = vec![
        Rc::new(RefCell::new(Miner::new(0, "A", genesis_block.clone(), 0.3))), // "A" for "Attacker"
        Rc::new(RefCell::new(Miner::new(1, "B", genesis_block.clone(), 0.3))), // "B" for "Big guy"
        Rc::new(RefCell::new(Miner::new(2, "C", genesis_block.clone(), 0.4))), // "C" for "Crud"
    ];

    // A -> B (0s), A -> C (5s)
    miners[0].borrow_mut().add_connection(&miners[1], 0);
    miners[0].borrow_mut().add_connection(&miners[2], 5);

    // B -> A (0s), B -> C (0s)
    miners[1].borrow_mut().add_connection(&miners[0], 0);
    miners[1].borrow_mut().add_connection(&miners[2], 0);

    // C -> A (0s), C -> B (0s)
    miners[2].borrow_mut().add_connection(&miners[0], 0);
    miners[2].borrow_mut().add_connection(&miners[1], 0);

    for t in 1..seconds_to_simulate {
        // Announcement phase
        for miner in &miners {
            miner.borrow_mut().send_messages();
        }
        // Mining phase
        for miner in &miners {
            miner.borrow_mut().mine(t);
        }
    }

    let labels = ["A (Attacker)", "B (Big guy)", "C (Crud)"];

    for (i, miner_rc) in miners.iter().enumerate() {
        let miner = miner_rc.borrow();

        println!("\nMiner {} - {}", i, labels[i]);
        println!("  Hashrate proportion: {:.1}%", miner.hashrate_proportion * 100.0);
        println!("  Current block height: {}", miner.current_block.height);
        println!("  Total known blocks: {}", miner.known_blocks.len());

        // Count blocks mined by this miner in their main chain
        let mut blocks_in_chain = 0usize;
        let mut block = miner.current_block.clone();
        while let Some(parent) = block.parent.as_ref().and_then(|w| w.upgrade()) {
            if block.miner_id == Some(miner.id) {
                blocks_in_chain += 1;
            }
            block = parent;
        }

        println!("  Blocks by this miner in main chain: {}", blocks_in_chain);
        println!("  Blocks found by this miner: {}", miner.blocks_mined);

        if miner.current_block.height > 0 {
            let pct = (blocks_in_chain as f64) / (miner.current_block.height as f64);
            println!("  Percentage of main chain: {:.4}%", pct * 100.0);
        }

        let stale_blocks = miner.blocks_mined.saturating_sub(blocks_in_chain);
        let stale_rate = if miner.blocks_mined > 0 {
            (stale_blocks as f64) / (miner.blocks_mined as f64)
        } else {
            0.0
        };
        println!("  Stale Blocks: {}", stale_blocks);
        println!("  Stale rate: {:.4}", stale_rate);
    }
}
