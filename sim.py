import argparse
import hashlib
import random
from dataclasses import dataclass
from math import exp
from typing import Optional

# Each connection is only in one direction!
class Connection:
    sender: 'Miner'
    receiver: 'Miner'
    delay: int

    @dataclass
    class QueueEntry:
        block: 'Block'
        tts: int

    # A queue of block messages to send, each with a countdown that is
    # initialized to the delay.
    blocks_to_send: list[QueueEntry]

    def __init__(self, sender: 'Miner', receiver: 'Miner', delay: int):
        self.blocks_to_send = []
        self.sender = sender
        self.receiver = receiver
        self.delay = delay

    def queue_block(self, block: 'Block'):
        self.blocks_to_send.append(self.QueueEntry(block, self.delay))

    def send_block(self, block: 'Block'):
        assert block.miner is not None
        # print(f"{self.sender.name} sending block at height {block.height} mined by {block.miner.name} to {self.receiver.name}")
        return self.receiver.receive_block(block)
        
    def tick(self):
        if len(self.blocks_to_send) == 0:
            return
        remaining = []
        for entry in self.blocks_to_send:
            if entry.tts == 0:
                self.send_block(entry.block)
            else:
                entry.tts -= 1
                remaining.append(entry)
        
        self.blocks_to_send = remaining

class Block:
    height: int
    time: int
    hash: str
    miner: Optional['Miner']
    parent: Optional['Block']

    def __init__(self, height:int, time:int, parent: Optional['Block'], miner: Optional['Miner']):
        self.height = height
        self.time = time
        self.miner = miner
        self.parent = parent
        parent_hash:str = parent.hash if parent is not None else "0" * 64
        preimage = f"{height}{miner}{parent_hash}"
        self.hash = hashlib.sha256(preimage.encode('utf-8')).hexdigest()

class Miner:
    # proportion of global hashrate
    hashrate_proportion: float
    current_block: Block
    name: str
    block_candidates: list[Block]
    connections: list[Connection]
    known_blocks: dict[str, Block]
    rejected_blocks: dict[str, Block]

    # just for the stats
    blocks_mined: int

    def __init__(self, name, initial_block: Block, hashrate_proportion: float):
        self.name = name
        self.blocks_mined = 0
        self.current_block = initial_block
        self.hashrate_proportion = hashrate_proportion
        self.connections = []
        self.known_blocks = {initial_block.hash: initial_block}
        self.rejected_blocks = {}
        self.block_candidates = []
        self.probability_per_second = self.hashrate_proportion * (1 - exp(-1/600))

    # Add connection to another miner with a given propagation delay.
    def add_connection(self, other: 'Miner', delay: int):
        self.connections.append(Connection(self, other, delay))

    def is_mine(self, block: Block):
        return block.miner == self

    # Sets current_block and clears block_candidates
    def evaluate_candidates(self):
        # Filter candidates to those with the maximum height
        max_height = max(block.height for block in self.block_candidates)
        candidates_max_height = [block for block in self.block_candidates if block.height == max_height]

        # Miners always pick their own for the same height.
        for candidate in candidates_max_height:
            if self.is_mine(candidate):
                self.current_block = candidate
                self.block_candidates = []
                return

        # Choose randomly
        self.current_block = random.choice(candidates_max_height)
        self.block_candidates = []

    def mine(self, time):
        if len(self.block_candidates) > 0:
            self.evaluate_candidates()
        if random.random() < self.probability_per_second:
            found_block = Block(self.current_block.height + 1, time, self.current_block, self)
            self.known_blocks[found_block.hash] = found_block
            self.block_candidates.append(found_block)
            self.announce(found_block)
            # print(f"{self.name} found a block at height: {found_block.height}")

    def announce(self, block: Block):
        self.blocks_mined += 1
        for connection in self.connections:
            connection.queue_block(block)

    def send_messages(self):
        for connection in self.connections:
            connection.tick()

    # This needs to happen any time we insert into known_blocks, could be more
    # performant probably with a block arg, but this is easier to reason about
    # by looping over everything.
    def refresh_rejects(self):
        moved: Optional[str] = None
        for reject_hash, reject_block in self.rejected_blocks.items():
            assert reject_block.parent is not None
            if reject_block.parent.hash in self.known_blocks:
                moved = reject_hash
                self.known_blocks[reject_hash] = reject_block
                break

        if moved is not None:
            self.rejected_blocks.pop(moved)
            # We need to check back through the reject pile,
            # since we just updated known_blocks.
            self.refresh_rejects()


    def receive_block(self, block: Block):
        # Only true for genesis, which we'll never receive.
        assert block.parent is not None

        # We've seen it before
        if block.hash in self.known_blocks:
            return

        # Reject blocks we don't have the chain for, this solves the following
        # complication: what if miner A's blocks have a 5s delay to us, and
        # miner B's blocks have 0s delay to us, but miner A finds a block and
        # miner B finds a child of that block before we have heard about A.
        if block.parent.hash not in self.known_blocks:
            self.rejected_blocks[block.hash] = block
            return

        self.known_blocks[block.hash] = block
        self.refresh_rejects()

        # Anything above the current height is a candidate, anything with the
        # same height as current block is not a candidate, since we must have
        # received current earlier.
        if block.height > self.current_block.height:
            self.block_candidates.append(block)

        
def main(block_periods_to_simulate: int):
    seconds_to_simulate = block_periods_to_simulate * 600
    genesis_block = Block(0, 0, None, None)
    miners = [
        Miner("A", genesis_block, 0.3), # "A" for "Attacker"
        Miner("B", genesis_block, 0.3), # "B" for "Big guy"
        Miner("C", genesis_block, 0.4)  # "C" for "Crud"
    ]

    miners[0].add_connection(miners[1], 0)
    miners[0].add_connection(miners[2], 5)

    miners[1].add_connection(miners[0], 0)
    miners[1].add_connection(miners[2], 0)

    miners[2].add_connection(miners[0], 0)
    miners[2].add_connection(miners[1], 0)

    for t in range(1, seconds_to_simulate):
        # First an announcement phase
        for miner in miners:
            miner.send_messages()
        # Second, a mining phase
        for miner in miners:
            miner.mine(t)
            

    for i, miner in enumerate(miners):
        label = ["A (Attacker)", "B (Big guy)", "C (Crud)"][i]
        print(f"\nMiner {i} - {label}")
        print(f"  Hashrate proportion: {miner.hashrate_proportion:.1%}")
        print(f"  Current block height: {miner.current_block.height}")
        print(f"  Total known blocks: {len(miner.known_blocks)}")
        
        # Count blocks mined by this miner in their main chain
        blocks_in_chain = 0
        block = miner.current_block
        while block.parent is not None:
            if block.miner == miner:
                blocks_in_chain += 1
            block = block.parent
        
        print(f"  Blocks by this miner in main chain: {blocks_in_chain}")
        print(f"  Blocks found by this miner: {miner.blocks_mined}")
        if miner.current_block.height > 0:
            print(f"  Percentage of main chain: {blocks_in_chain / miner.current_block.height:.4%}")
        stale_blocks = miner.blocks_mined - blocks_in_chain
        stale_rate = stale_blocks / miner.blocks_mined
        print(f"  Stale Blocks: {stale_blocks}")
        print(f"  Stale rate: {stale_rate:.4f}")


if __name__ == "__main__":
    import random

    parser = argparse.ArgumentParser(description="Block propagation/mining simulator")
    parser.add_argument("block_periods", type=int, nargs="?", default=10000,
                        help="Number of 600 sec block periods to simulate (default: 10000)")

    args = parser.parse_args()

    # Run the simulation
    main(args.block_periods)
