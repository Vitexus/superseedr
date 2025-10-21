// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::torrent_manager::state::TorrentStatus;

use rand::prelude::IndexedRandom;

use tracing::{event, Level};

use std::collections::HashMap;
use std::collections::HashSet;

#[derive(PartialEq, Clone, Copy, Debug)]
pub enum PieceStatus {
    Need,
    Done,
}

pub struct PieceAssembler {
    buffer: Vec<u8>,
    received_blocks: HashSet<u32>, // Store block offsets
    total_blocks: usize,
}

#[derive(Default)]
pub struct PieceManager {
    pub bitfield: Vec<PieceStatus>,
    pub need_queue: Vec<u32>,
    pub pending_queue: HashMap<u32, Vec<String>>,
    pub piece_rarity: HashMap<u32, usize>,
    pub pieces_remaining: usize,
    pub piece_assemblers: HashMap<u32, PieceAssembler>,
}

impl PieceManager {
    pub fn new() -> Self {
        Self {
            bitfield: Vec::new(),
            need_queue: Vec::new(),
            pending_queue: HashMap::new(),
            piece_rarity: HashMap::new(),
            pieces_remaining: 0,
            piece_assemblers: HashMap::new(),
        }
    }

    pub fn set_initial_fields(&mut self, num_pieces: usize, validation_complete: bool) {
        let mut bitfield = vec![PieceStatus::Need; num_pieces];

        if validation_complete {
            bitfield.fill(PieceStatus::Done);
        } else {
            for (i, status) in bitfield.iter().enumerate() {
                if *status == PieceStatus::Need {
                    self.need_queue.push(i as u32);
                }
            }
        }
        self.bitfield = bitfield;
        self.pieces_remaining = self.need_queue.len();
    }

    pub fn choose_piece_for_peer(
        &self,
        peer_bitfield: &[bool],
        peer_pending: &HashSet<u32>,
        torrent_status: &TorrentStatus,
    ) -> Option<u32> {
        if *torrent_status != TorrentStatus::Endgame {
            // --- STANDARD MODE: Rarest First ---
            self.need_queue
                .iter()
                .filter(|&&piece_idx| peer_bitfield.get(piece_idx as usize) == Some(&true))
                .filter(|&&piece_idx| !peer_pending.contains(&piece_idx))
                .min_by_key(|&&piece_idx| self.piece_rarity.get(&piece_idx).unwrap_or(&usize::MAX))
                .copied()
        } else {
            // --- ENDGAME MODE: Random from Pending ---
            let candidate_pieces: Vec<u32> = self
                .pending_queue
                .keys()
                .chain(self.need_queue.iter())
                .filter(|&&piece_idx| peer_bitfield.get(piece_idx as usize) == Some(&true))
                .filter(|&&piece_idx| !peer_pending.contains(&piece_idx))
                .copied()
                .collect();

            // Choose a random piece from the candidates.
            candidate_pieces.choose(&mut rand::rng()).copied()
        }
    }

    pub fn mark_as_pending(&mut self, piece_index: u32, peer_id: String) {
        self.need_queue.retain(|&p| p != piece_index);
        self.pending_queue
            .entry(piece_index)
            .or_default()
            .push(peer_id.clone());
    }

    pub fn requeue_pending_to_need(&mut self, piece_index: u32) {
        self.pending_queue.remove(&piece_index);
        self.need_queue.push(piece_index);
    }

    pub fn mark_as_complete(&mut self, piece_index: u32) -> Vec<String> {
        if self.bitfield.get(piece_index as usize) == Some(&PieceStatus::Done) {
            return Vec::new(); // Already complete, nothing to do.
        }

        self.bitfield[piece_index as usize] = PieceStatus::Done;
        self.pieces_remaining -= 1;
        self.need_queue.retain(|&p| p != piece_index);

        self.pending_queue.remove(&piece_index).unwrap_or_default()
    }

    pub fn reset_piece_assembly(&mut self, piece_index: u32) {
        // Simply remove the assembler. The next block to arrive for this piece
        // will trigger the creation of a new, clean assembler.
        self.piece_assemblers.remove(&piece_index);
        event!(
            Level::DEBUG,
            piece = piece_index,
            "Resetting piece assembler due to verification failure."
        );
    }

    pub fn update_rarity<'a, I>(&mut self, all_peer_bitfields: I)
    where
        I: Iterator<Item = &'a Vec<bool>> + Clone, // Clone is needed because we iterate multiple times
    {
        self.piece_rarity.clear();
        let pieces_to_check: Vec<u32> = self
            .need_queue
            .iter()
            .chain(self.pending_queue.keys())
            .copied()
            .collect();

        for piece_idx in pieces_to_check {
            let count = all_peer_bitfields
                .clone() // This is a cheap clone of the iterator, not the data
                .filter(|p_bitfield| p_bitfield.get(piece_idx as usize) == Some(&true))
                .count();
            self.piece_rarity.insert(piece_idx, count);
        }
    }

    pub fn handle_block(
        &mut self,
        piece_index: u32,
        block_offset: u32,
        block_data: &[u8],
        piece_size: usize,
    ) -> Option<Vec<u8>> {
        // Get or create the assembler for this piece
        let assembler = self.piece_assemblers.entry(piece_index).or_insert_with(|| {
            let total_blocks = (piece_size as f64 / 16384.0).ceil() as usize;
            PieceAssembler {
                buffer: vec![0; piece_size],
                received_blocks: HashSet::new(),
                total_blocks,
            }
        });

        let start = block_offset as usize;
        let max_end = piece_size;
        let calculated_end = start.saturating_add(block_data.len());
        let actual_end = std::cmp::min(calculated_end, max_end);
        let copy_len = actual_end.saturating_sub(start);

        if copy_len > 0 && start < max_end {
            let data_to_copy = &block_data[..copy_len];
            assembler.buffer[start..actual_end].copy_from_slice(data_to_copy);
            assembler.received_blocks.insert(block_offset);
        }

        // Check if the piece is complete
        if assembler.received_blocks.len() == assembler.total_blocks {
            // It's complete! Remove it from the map and return the data.
            if let Some(finished_assembler) = self.piece_assemblers.remove(&piece_index) {
                return Some(finished_assembler.buffer);
            }
        }

        // Not complete yet
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::torrent_manager::state::TorrentStatus; // Make sure this path is correct
    use std::collections::HashSet;

    /// Helper to create a piece manager initialized with 'Need' pieces
    fn setup_manager(num_pieces: usize) -> PieceManager {
        let mut pm = PieceManager::new();
        pm.set_initial_fields(num_pieces, false);
        pm
    }

    #[test]
    fn test_initialization_not_validated() {
        let mut pm = PieceManager::new();
        let num_pieces = 10;
        pm.set_initial_fields(num_pieces, false);

        assert_eq!(pm.bitfield.len(), num_pieces);
        assert_eq!(pm.bitfield[0], PieceStatus::Need);
        assert_eq!(pm.need_queue.len(), num_pieces);
        assert_eq!(pm.pieces_remaining, num_pieces);
        assert_eq!(pm.need_queue[0], 0);
        assert_eq!(pm.need_queue[9], 9);
    }

    #[test]
    fn test_initialization_pre_validated() {
        let mut pm = PieceManager::new();
        let num_pieces = 10;
        pm.set_initial_fields(num_pieces, true);

        assert_eq!(pm.bitfield.len(), num_pieces);
        assert_eq!(pm.bitfield[0], PieceStatus::Done);
        assert!(pm.need_queue.is_empty());
        assert_eq!(pm.pieces_remaining, 0);
    }

    #[test]
    fn test_state_transitions() {
        let mut pm = setup_manager(5); // pieces 0, 1, 2, 3, 4
        assert_eq!(pm.pieces_remaining, 5);
        assert_eq!(pm.need_queue, vec![0, 1, 2, 3, 4]);

        // 1. Mark piece 2 as PENDING
        pm.mark_as_pending(2, "peer_A".to_string());
        assert_eq!(pm.need_queue, vec![0, 1, 3, 4]);
        assert_eq!(
            pm.pending_queue.get(&2).unwrap(),
            &vec!["peer_A".to_string()]
        );
        assert_eq!(pm.pieces_remaining, 5); // Still need it

        // 2. Mark piece 2 as PENDING from another peer
        pm.mark_as_pending(2, "peer_B".to_string());
        assert_eq!(
            pm.pending_queue.get(&2).unwrap(),
            &vec!["peer_A".to_string(), "peer_B".to_string()]
        );

        // 3. Requeue piece 2 back to NEED
        pm.requeue_pending_to_need(2);
        // Order doesn't matter, check presence and absence
        assert!(!pm.pending_queue.contains_key(&2));
        assert!(pm.need_queue.contains(&0));
        assert!(pm.need_queue.contains(&1));
        assert!(pm.need_queue.contains(&2));
        assert!(pm.need_queue.contains(&3));
        assert!(pm.need_queue.contains(&4));
        assert_eq!(pm.need_queue.len(), 5);

        // 4. Mark piece 3 (from NEED) as COMPLETE
        let peers_to_cancel = pm.mark_as_complete(3);
        assert!(peers_to_cancel.is_empty());
        assert_eq!(pm.bitfield[3], PieceStatus::Done);
        assert_eq!(pm.pieces_remaining, 4);
        assert!(!pm.need_queue.contains(&3));

        // 5. Mark piece 2 (from PENDING) as COMPLETE
        pm.mark_as_pending(2, "peer_C".to_string()); // Pend it again
        let peers_to_cancel = pm.mark_as_complete(2);
        assert_eq!(peers_to_cancel, vec!["peer_C".to_string()]);
        assert_eq!(pm.bitfield[2], PieceStatus::Done);
        assert_eq!(pm.pieces_remaining, 3);
        assert!(!pm.pending_queue.contains_key(&2));
        assert!(!pm.need_queue.contains(&2));

        // 6. Mark piece 2 (already DONE) as COMPLETE (idempotent)
        let peers_to_cancel = pm.mark_as_complete(2);
        assert!(peers_to_cancel.is_empty());
        assert_eq!(pm.pieces_remaining, 3); // No change
    }

    #[test]
    fn test_piece_assembly_and_reset() {
        let mut pm = PieceManager::new();
        let piece_index = 0;
        let piece_size = 32768; // 2 blocks of 16384
        let block_size = 16384;

        let block_data_0 = vec![1; block_size];
        let block_data_1 = vec![2; block_size];

        // 1. Add first block
        let result = pm.handle_block(piece_index, 0, &block_data_0, piece_size);
        assert!(result.is_none());
        assert!(pm.piece_assemblers.contains_key(&piece_index));
        let assembler = pm.piece_assemblers.get(&piece_index).unwrap();
        assert_eq!(assembler.total_blocks, 2);
        assert_eq!(assembler.received_blocks.len(), 1);

        // 2. Reset the assembler (e.g., hash fail)
        pm.reset_piece_assembly(piece_index);
        assert!(!pm.piece_assemblers.contains_key(&piece_index));

        // 3. Add first block again (new assembler created)
        let result = pm.handle_block(piece_index, 0, &block_data_0, piece_size);
        assert!(result.is_none());

        // 4. Add second block
        let result = pm.handle_block(piece_index, block_size as u32, &block_data_1, piece_size);

        // 5. Check completion
        assert!(result.is_some());
        let full_piece = result.unwrap();
        assert_eq!(full_piece.len(), piece_size);
        assert_eq!(&full_piece[0..block_size], &block_data_0[..]);
        assert_eq!(&full_piece[block_size..], &block_data_1[..]);

        // 6. Assembler should be gone
        assert!(!pm.piece_assemblers.contains_key(&piece_index));
    }

    #[test]
    fn test_update_rarity() {
        let mut pm = setup_manager(4); // need = [0, 1, 2, 3]
        pm.mark_as_pending(2, "peer_A".to_string()); // need = [0, 1, 3], pending = [2]
        pm.mark_as_complete(0); // need = [1, 3], pending = [2], done = [0]
                                // Pieces to check: 1, 3, 2

        let peer1_bitfield = vec![true, true, false, true]; // Has 0, 1, 3
        let peer2_bitfield = vec![true, false, true, true]; // Has 0, 2, 3
        let peer_bitfields = vec![peer1_bitfield, peer2_bitfield];

        pm.update_rarity(peer_bitfields.iter());

        // Piece 0 is Done, should not be in rarity map
        assert!(!pm.piece_rarity.contains_key(&0));
        // Piece 1 is Need, 1 peer has it
        assert_eq!(pm.piece_rarity.get(&1), Some(&1));
        // Piece 2 is Pending, 1 peer has it
        assert_eq!(pm.piece_rarity.get(&2), Some(&1));
        // Piece 3 is Need, 2 peers have it
        assert_eq!(pm.piece_rarity.get(&3), Some(&2));
    }

    #[test]
    fn test_choose_piece_standard_mode() {
        let mut pm = setup_manager(5); // need = [0, 1, 2, 3, 4]

        // Rarity: 0 (rare), 1 (common), 2 (rare), 3 (medium), 4 (peer doesn't have)
        pm.piece_rarity.insert(0, 1);
        pm.piece_rarity.insert(1, 10);
        pm.piece_rarity.insert(2, 1);
        pm.piece_rarity.insert(3, 5);
        pm.piece_rarity.insert(4, 2);

        let peer_bitfield = vec![true, true, true, true, false]; // Has 0, 1, 2, 3
        let mut peer_pending = HashSet::new();
        let status = TorrentStatus::Standard;

        // 1. Choose rarest piece
        // Peer has [0, 1, 2, 3]. Rarity [0:1, 1:10, 2:1, 3:5]
        // Rarest are 0 and 2. `min_by_key` is stable, but either is fine.
        let choice = pm.choose_piece_for_peer(&peer_bitfield, &peer_pending, &status);
        assert!(choice == Some(0) || choice == Some(2));
        let chosen_piece = choice.unwrap();

        // 2. Choose rarest, but chosen piece (0 or 2) is now pending for this peer
        peer_pending.insert(chosen_piece);
        // Candidates [1, 3] if 0/2 was chosen. Rarity [1:10, 3:5]. Rarest is 3.
        // OR Candidates [0, 1, 3] if 2 was chosen. Rarity [0:1, 1:10, 3:5]. Rarest is 0.
        // OR Candidates [1, 2, 3] if 0 was chosen. Rarity [1:10, 2:1, 3:5]. Rarest is 2.
        let choice2 = pm.choose_piece_for_peer(&peer_bitfield, &peer_pending, &status);
        if chosen_piece == 0 {
            assert_eq!(choice2, Some(2));
        } else {
            assert_eq!(choice2, Some(0));
        } // If chosen_piece == 2

        // 3. Make all available pieces pending for this peer
        peer_pending.insert(0);
        peer_pending.insert(1);
        peer_pending.insert(2);
        peer_pending.insert(3);
        // Peer has [0, 1, 2, 3]. Pending [0, 1, 2, 3]. No candidates.
        let choice = pm.choose_piece_for_peer(&peer_bitfield, &peer_pending, &status);
        assert_eq!(choice, None);

        // 4. Peer has nothing we need
        let empty_peer_bitfield = vec![false; 5];
        let choice = pm.choose_piece_for_peer(&empty_peer_bitfield, &peer_pending, &status);
        assert_eq!(choice, None);
    }

    #[test]
    fn test_choose_piece_endgame_mode_prioritizes_pending() {
        let mut pm = setup_manager(5); // need = [0, 1, 2, 3, 4]
        pm.mark_as_pending(1, "peer_A".to_string()); // need = [0, 2, 3, 4], pending = [1]
        pm.mark_as_pending(2, "peer_B".to_string()); // need = [0, 3, 4], pending = [1, 2]

        let peer_bitfield = vec![true, true, true, true, false]; // Has 0, 1, 2, 3
        let peer_pending = HashSet::new();
        let status = TorrentStatus::Endgame;

        // Peer has pieces Need[0, 3] and Pending[1, 2]. All are candidates.
        // Run multiple times to increase chance of seeing different random choices.
        let mut choices = HashSet::new();
        for _ in 0..20 {
            let choice = pm
                .choose_piece_for_peer(&peer_bitfield, &peer_pending, &status)
                .unwrap();
            assert!([0, 1, 2, 3].contains(&choice));
            choices.insert(choice);
        }
        // Check if we got at least one from Need and one from Pending over several tries.
        assert!(choices.contains(&0) || choices.contains(&3)); // Need
        assert!(choices.contains(&1) || choices.contains(&2)); // Pending
    }

    #[test]
    fn test_choose_piece_endgame_mode_excludes_peer_pending() {
        let mut pm = setup_manager(5); // need = [0, 1, 2, 3, 4]
        pm.mark_as_pending(1, "peer_A".to_string()); // need = [0, 2, 3, 4], pending = [1]
        pm.mark_as_pending(2, "peer_B".to_string()); // need = [0, 3, 4], pending = [1, 2]

        let peer_bitfield = vec![true, true, true, true, false]; // Has 0, 1, 2, 3
        let mut peer_pending = HashSet::new();
        peer_pending.insert(1); // Peer is already downloading piece 1
        let status = TorrentStatus::Endgame;

        // Candidates should be [0, 2, 3] (excludes piece 1)
        for _ in 0..20 {
            let choice = pm
                .choose_piece_for_peer(&peer_bitfield, &peer_pending, &status)
                .unwrap();
            assert!([0, 2, 3].contains(&choice));
            assert_ne!(choice, 1);
        }
    }

    // --- Tests for handle_block ---

    #[test]
    fn test_handle_block_out_of_order() {
        let mut pm = PieceManager::new();
        let piece_index = 0;
        let piece_size = 32768; // 2 blocks
        let block_size = 16384;
        let block_data_0 = vec![1; block_size];
        let block_data_1 = vec![2; block_size];

        // Receive block 1 first
        let result1 = pm.handle_block(piece_index, block_size as u32, &block_data_1, piece_size);
        assert!(result1.is_none());
        assert!(pm.piece_assemblers.contains_key(&piece_index));
        let assembler1 = pm.piece_assemblers.get(&piece_index).unwrap();
        assert_eq!(assembler1.received_blocks.len(), 1);
        assert!(assembler1.received_blocks.contains(&(block_size as u32)));

        // Receive block 0 second
        let result0 = pm.handle_block(piece_index, 0, &block_data_0, piece_size);
        assert!(result0.is_some()); // Should complete now
        let full_piece = result0.unwrap();
        assert_eq!(full_piece.len(), piece_size);
        assert_eq!(&full_piece[0..block_size], &block_data_0[..]);
        assert_eq!(&full_piece[block_size..], &block_data_1[..]);
        assert!(!pm.piece_assemblers.contains_key(&piece_index)); // Assembler gone
    }

    #[test]
    fn test_handle_block_duplicate() {
        let mut pm = PieceManager::new();
        let piece_index = 0;
        let piece_size = 16384; // 1 block
        let block_size = 16384;
        let block_data = vec![1; block_size];

        // Receive block 0
        let result1 = pm.handle_block(piece_index, 0, &block_data, piece_size);
        assert!(result1.is_some()); // Complete on first block
        assert!(!pm.piece_assemblers.contains_key(&piece_index));

        // Receive block 0 again (should be ignored gracefully)
        // Need to manually create an assembler context if we expect handle_block
        // to operate on an existing assembler. If the piece is already complete,
        // handle_block might just return None immediately.
        // Let's test the state *during* assembly.
        let piece_size_2 = 32768; // 2 blocks
        let block_data_0 = vec![1; block_size];
        let block_data_1 = vec![2; block_size];

        // Add block 0
        pm.handle_block(1, 0, &block_data_0, piece_size_2);
        assert!(pm.piece_assemblers.contains_key(&1));
        let assembler1 = pm.piece_assemblers.get(&1).unwrap();
        assert_eq!(assembler1.received_blocks.len(), 1);

        // Add block 0 again
        pm.handle_block(1, 0, &block_data_0, piece_size_2);
        assert!(pm.piece_assemblers.contains_key(&1));
        let assembler2 = pm.piece_assemblers.get(&1).unwrap();
        assert_eq!(assembler2.received_blocks.len(), 1); // Length should not increase

        // Add block 1 to complete
        let result_final = pm.handle_block(1, block_size as u32, &block_data_1, piece_size_2);
        assert!(result_final.is_some());
        assert!(!pm.piece_assemblers.contains_key(&1));
    }

    #[test]
    fn test_handle_block_for_completed_piece() {
        let mut pm = setup_manager(1);
        let piece_index = 0;
        let piece_size = 16384;
        let block_data = vec![1; piece_size];

        // Mark piece as complete first
        pm.mark_as_complete(piece_index);
        assert_eq!(pm.bitfield[piece_index as usize], PieceStatus::Done);

        // Handle a block for the completed piece
        // We expect handle_block might create an assembler temporarily
        // but it shouldn't return the piece data again.
        // Let's refine the expectation: If the piece manager knows the piece is Done,
        // `handle_block` might ideally check this first and do nothing.
        // If it relies solely on the assembler map, it might reassemble.
        // Current implementation relies on assembler map.

        // Reset assembler state for the test
        pm.piece_assemblers.remove(&piece_index);

        let result = pm.handle_block(piece_index, 0, &block_data, piece_size);
        // Even though it assembles, because the `mark_as_complete` call removed
        // it from need/pending queues, the manager logic *outside* handle_block
        // should prevent requesting it again. The assembler map is primarily for
        // in-progress downloads.
        assert!(result.is_some()); // It will reassemble based on current logic
        assert!(!pm.piece_assemblers.contains_key(&piece_index)); // Assembler is removed on completion
    }

    #[test]
    fn test_handle_block_non_standard_piece_size() {
        let mut pm = PieceManager::new();
        let piece_index = 0;
        let piece_size = 20000; // Not a multiple of 16384
        let block_size = 16384;

        let block_data_0 = vec![1; block_size];
        let block_data_1 = vec![2; piece_size - block_size]; // Remaining size

        let total_blocks_expected = (piece_size as f64 / block_size as f64).ceil() as usize;
        assert_eq!(total_blocks_expected, 2);

        // Add first block
        let result0 = pm.handle_block(piece_index, 0, &block_data_0, piece_size);
        assert!(result0.is_none());
        assert!(pm.piece_assemblers.contains_key(&piece_index));
        let assembler = pm.piece_assemblers.get(&piece_index).unwrap();
        assert_eq!(assembler.total_blocks, total_blocks_expected);
        assert_eq!(assembler.received_blocks.len(), 1);

        // Add second (partial) block
        let result1 = pm.handle_block(piece_index, block_size as u32, &block_data_1, piece_size);
        assert!(result1.is_some());
        let full_piece = result1.unwrap();
        assert_eq!(full_piece.len(), piece_size);
        assert_eq!(&full_piece[0..block_size], &block_data_0[..]);
        assert_eq!(&full_piece[block_size..], &block_data_1[..]);
        assert!(!pm.piece_assemblers.contains_key(&piece_index));
    }

    #[test]
    fn test_handle_block_ignores_extra_data() {
        let mut pm = PieceManager::new();
        let piece_index = 0;
        let piece_size = 16384; // Exactly one block
        let block_size = 16384;
        let correct_block_data = vec![1; block_size];
        let oversized_block_data = vec![1; block_size + 10]; // Extra data

        // Send oversized block
        let result = pm.handle_block(piece_index, 0, &oversized_block_data, piece_size);

        // It should still complete, but only using the expected size
        assert!(result.is_some());
        let full_piece = result.unwrap();
        assert_eq!(full_piece.len(), piece_size);
        assert_eq!(full_piece, correct_block_data); // Ensure only correct data was stored
        assert!(!pm.piece_assemblers.contains_key(&piece_index));
    }
}
