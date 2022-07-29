// Copyright 2022 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

/// BTree iterator implementation.
/// This does not enforce consistency.
/// If a commit did happen after an
/// iterator is created, the iterator will
/// just replay a `seek` operation at its
/// latest accessed key.u
use super::*;
use crate::{
	btree::BTreeTable, db::CommitOverlay, error::Result, log::LogQuery, table::key::TableKeyQuery,
};
use parking_lot::RwLock;

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Copy)]
pub enum SeekTo {
	Include,
	Exclude,
}

pub struct BTreeIterator<'a> {
	table: &'a BTreeTable,
	log: &'a RwLock<crate::log::LogOverlays>,
	commit_overlay: &'a RwLock<Vec<CommitOverlay>>,
	iter: BtreeIterBackend,
	col: ColId,
	pending_backend: Option<(Option<(Vec<u8>, Vec<u8>)>, Option<IterDirection>)>,
	last_key: LastKey,
}

pub enum LastKey {
	Start,
	End,
	At(Vec<u8>),
	Seeked(Vec<u8>),
}

#[derive(Debug)]
pub enum LastIndex {
	Start,
	End,
	Seeked(usize),
	At(usize),
	Before(usize),
	After(usize),
	Descend(usize),
}

pub struct BtreeIterBackend(BTree, BTreeIterState);

impl<'a> BTreeIterator<'a> {
	pub(crate) fn new(
		table: &'a BTreeTable,
		col: ColId,
		log: &'a RwLock<crate::log::LogOverlays>,
		commit_overlay: &'a RwLock<Vec<CommitOverlay>>,
	) -> Result<Self> {
		let record_id = log.read().last_record_id(col);
		let tree = table.with_locked(|btree| BTree::open(btree, log, record_id))?;
		let iter = BTreeIterState::new(tree.record_id);
		Ok(BTreeIterator {
			table,
			iter: BtreeIterBackend(tree, iter),
			col,
			pending_backend: None,
			last_key: LastKey::Start,
			log,
			commit_overlay,
		})
	}

	pub fn seek(&mut self, key: &[u8]) -> Result<()> {
		// seek require log do not change
		let log = self.log.read();
		let record_id = log.last_record_id(self.col);
		self.last_key = LastKey::Seeked(key.to_vec());
		self.pending_backend = None;
		self.seek_backend(
			key,
			record_id,
			self.table,
			&*log,
			SeekTo::Include,
			IterDirection::Forward,
		)
	}

	pub fn seek_to_first(&mut self) -> Result<()> {
		self.seek(&[])
	}

	pub fn seek_to_last(&mut self) -> Result<()> {
		let log = self.log.read();
		let record_id = log.last_record_id(self.col);
		self.last_key = LastKey::End;
		self.seek_backend_to_last(record_id, self.table, &*log)
	}

	#[allow(clippy::should_implement_trait)]
	pub fn next(&mut self) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
		self.iter_inner(IterDirection::Forward)
	}

	pub fn prev(&mut self) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
		self.iter_inner(IterDirection::Backward)
	}

	fn iter_inner(&mut self, direction: IterDirection) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
		let col = self.col;

		// Lock log over function call (no btree struct change).
		let commit_overlay = self.commit_overlay.read();
		let next_commit_overlay = commit_overlay.get(col as usize).and_then(|o| match direction {
			IterDirection::Forward => o.btree_next(&self.last_key),
			IterDirection::Backward => o.btree_prev(&self.last_key),
		});
		let log = self.log.read();
		let record_id = log.last_record_id(self.col);
		// No consistency over iteration, allows dropping lock to overlay.
		std::mem::drop(commit_overlay);
		if record_id != self.iter.1.record_id {
			self.pending_backend = None;
		}
		let next_backend = if let Some((backend, prev_direction)) = self.pending_backend.take() {
			if prev_direction.map(|d| d != direction).unwrap_or(true) {
				// commit overlay last return, being in between, simply dropping pending.
				self.next_backend(record_id, self.table, &*log, direction)?
			} else {
				backend
			}
		} else {
			self.next_backend(record_id, self.table, &*log, direction)?
		};
		let result = match (next_commit_overlay, next_backend) {
			(Some((commit_key, commit_value)), Some((backend_key, backend_value))) =>
				match (direction, commit_key.cmp(&backend_key)) {
					(IterDirection::Backward, std::cmp::Ordering::Greater) |
					(IterDirection::Forward, std::cmp::Ordering::Less) => {
						self.pending_backend =
							Some((Some((backend_key, backend_value)), Some(direction)));
						if let Some(value) = commit_value {
							Some((commit_key, value))
						} else {
							std::mem::drop(log);
							self.last_key = LastKey::At(commit_key.clone());
							self.iter_inner(direction)?
						}
					},
					(IterDirection::Backward, std::cmp::Ordering::Less) |
					(IterDirection::Forward, std::cmp::Ordering::Greater) => Some((backend_key, backend_value)),
					(_, std::cmp::Ordering::Equal) =>
						if let Some(value) = commit_value {
							Some((backend_key, value))
						} else {
							std::mem::drop(log);
							self.last_key = LastKey::At(commit_key.clone());
							self.iter_inner(direction)?
						},
				},
			(Some((commit_key, Some(commit_value))), None) => {
				self.pending_backend = Some((None, Some(direction)));
				Some((commit_key, commit_value))
			},
			(Some((k, None)), None) => {
				self.pending_backend = Some((None, Some(direction)));
				std::mem::drop(log);
				self.last_key = LastKey::At(k.clone());
				self.iter_inner(direction)?
			},
			(None, Some((backend_key, backend_value))) => Some((backend_key, backend_value)),
			(None, None) => {
				self.pending_backend = Some((None, None));
				None
			},
		};

		match result.as_ref() {
			Some((key, _)) => {
				self.last_key = LastKey::At(key.clone());
			},
			None =>
				self.last_key = match direction {
					IterDirection::Backward => LastKey::Start,
					IterDirection::Forward => LastKey::End,
				},
		}
		Ok(result)
	}

	pub fn next_backend(
		&mut self,
		record_id: u64,
		col: &BTreeTable,
		log: &impl LogQuery,
		direction: IterDirection,
	) -> Result<Option<(Vec<u8>, Value)>> {
		let BtreeIterBackend(tree, iter) = &mut self.iter;
		if record_id != tree.record_id {
			let new_tree = col.with_locked(|btree| BTree::open(btree, log, record_id))?;
			*tree = new_tree;
			match &self.last_key {
				LastKey::At(last_key) => {
					iter.seek(last_key.as_slice(), tree, col, log, SeekTo::Exclude, direction)?;
				},
				LastKey::Seeked(last_key) => {
					iter.seek(
						last_key.as_slice(),
						tree,
						col,
						log,
						SeekTo::Include,
						IterDirection::Forward,
					)?;
				},
				LastKey::Start => {
					iter.seek(&[], tree, col, log, SeekTo::Include, IterDirection::Forward)?;
				},
				LastKey::End => {
					iter.seek_to_last(tree, col, log)?;
				},
			}
			iter.record_id = record_id;
		}

		iter.next(tree, col, log, direction)
	}

	pub fn seek_backend(
		&mut self,
		key: &[u8],
		record_id: u64,
		col: &BTreeTable,
		log: &impl LogQuery,
		seek_to: SeekTo,
		direction: IterDirection,
	) -> Result<()> {
		let BtreeIterBackend(tree, iter) = &mut self.iter;
		if record_id != tree.record_id {
			let new_tree = col.with_locked(|btree| BTree::open(btree, log, record_id))?;
			*tree = new_tree;
			iter.record_id = record_id;
		}
		iter.seek(key, tree, col, log, seek_to, direction)
	}

	pub fn seek_backend_to_last(
		&mut self,
		record_id: u64,
		col: &BTreeTable,
		log: &impl LogQuery,
	) -> Result<()> {
		let BtreeIterBackend(tree, iter) = &mut self.iter;
		if record_id != tree.record_id {
			let new_tree = col.with_locked(|btree| BTree::open(btree, log, record_id))?;
			*tree = new_tree;
			iter.record_id = record_id;
		}
		iter.seek_to_last(tree, col, log)
	}
}

#[derive(Debug)]
pub struct BTreeIterState {
	state: Vec<(LastIndex, Node)>,
	fetch_root: bool,
	pub record_id: u64,
}

impl BTreeIterState {
	fn enter(&mut self, at: usize, node: Node, direction: IterDirection) {
		if let Some((ix, _)) = self.state.last_mut() {
			*ix = LastIndex::Descend(at);
		}
		self.state.push((direction.starting(), node))
	}

	fn exit(&mut self, direction: IterDirection) -> bool {
		loop {
			if self.state.len() < 2 {
				// keep root
				if let Some((ix, _)) = self.state.last_mut() {
					*ix = match direction {
						IterDirection::Forward => LastIndex::End,
						IterDirection::Backward => LastIndex::Start,
					};
				}
				return true
			}
			self.state.pop();
			if let Some((ix, node)) = self.state.last_mut() {
				debug_assert!(matches!(ix, LastIndex::Descend(_)));
				if let LastIndex::Descend(child) = ix {
					*ix = match direction {
						IterDirection::Backward if *child == 0 => continue,

						IterDirection::Backward => LastIndex::After(*child - 1),
						IterDirection::Forward
							if *child == ORDER || node.separators[*child].separator.is_none() =>
							continue,
						IterDirection::Forward => LastIndex::Before(*child),
					};
					break
				} else {
					self.state.clear(); // should actually be unreachable
					self.fetch_root = false;
					break
				}
			}
		}
		false
	}

	pub fn new(record_id: u64) -> BTreeIterState {
		BTreeIterState { state: vec![], fetch_root: true, record_id }
	}

	pub fn next(
		&mut self,
		btree: &mut BTree,
		col: &BTreeTable,
		log: &impl LogQuery,
		direction: IterDirection,
	) -> Result<Option<(Vec<u8>, Value)>> {
		if !self.fetch_root && self.state.is_empty() {
			return Ok(None)
		}

		if self.fetch_root {
			let root = col.with_locked(|tables| {
				BTree::fetch_root(btree.root_index.unwrap_or(NULL_ADDRESS), tables, log)
			})?;
			self.state.push((direction.starting(), root));
			self.fetch_root = false;
		}

		loop {
			let is_leaf = btree.depth as usize + 1 == self.state.len();
			if let Some(state) = self.state.last_mut() {
				let next = match (direction, &state.0) {
					(_, LastIndex::Descend(_)) => unreachable!("exit function clean it"),
					(IterDirection::Forward, LastIndex::Start) if is_leaf => LastIndex::At(0),
					(IterDirection::Forward, LastIndex::Start) => LastIndex::Descend(0),
					(IterDirection::Forward, LastIndex::At(sep)) |
					(IterDirection::Forward, LastIndex::After(sep))
						if is_leaf && *sep + 1 == ORDER =>
					{
						if self.exit(direction) {
							break
						}
						continue
					},
					(IterDirection::Forward, LastIndex::At(sep)) |
					(IterDirection::Forward, LastIndex::After(sep))
						if is_leaf =>
						LastIndex::At(*sep + 1),
					(IterDirection::Forward, LastIndex::At(sep)) |
					(IterDirection::Forward, LastIndex::After(sep)) => LastIndex::Descend(*sep + 1),
					// TODO this check should not be needed: should be End
					(IterDirection::Forward, LastIndex::Seeked(sep)) |
					(IterDirection::Forward, LastIndex::Before(sep))
						if *sep == ORDER =>
					{
						if self.exit(direction) {
							break
						}
						continue
					},
					(IterDirection::Forward, LastIndex::Seeked(sep)) |
					(IterDirection::Forward, LastIndex::Before(sep)) => LastIndex::At(*sep),
					(IterDirection::Forward, LastIndex::End) => {
						if self.exit(direction) {
							break
						}
						continue
					},
					(IterDirection::Backward, LastIndex::End) if is_leaf => {
						if let Some(at) = state.1.last_separator_index() {
							LastIndex::At(at)
						} else {
							if self.exit(direction) {
								break
							}
							continue
						}
					},
					(IterDirection::Backward, LastIndex::End) => {
						if let Some(at) = state.1.last_separator_index() {
							LastIndex::Descend(at + 1)
						} else {
							if self.exit(direction) {
								break
							}
							continue
						}
					},
					(IterDirection::Backward, LastIndex::At(sep)) |
					(IterDirection::Backward, LastIndex::Before(sep))
						if is_leaf && *sep == 0 =>
					{
						if self.exit(direction) {
							break
						}
						continue
					},
					(IterDirection::Backward, LastIndex::At(sep)) |
					(IterDirection::Backward, LastIndex::Before(sep))
						if is_leaf =>
						LastIndex::At(*sep - 1),
					(IterDirection::Backward, LastIndex::At(sep)) |
					(IterDirection::Backward, LastIndex::Before(sep)) => LastIndex::Descend(*sep),
					(IterDirection::Backward, LastIndex::Seeked(sep)) |
					(IterDirection::Backward, LastIndex::After(sep)) => LastIndex::At(*sep),
					(IterDirection::Backward, LastIndex::Start) => {
						if self.exit(direction) {
							break
						}
						continue
					},
				};
				match next {
					LastIndex::At(at) => {
						if let Some(address) = state.1.separator_address(at) {
							state.0 = LastIndex::At(at);
							let key = state.1.separator_key(at).unwrap();
							let key_query = TableKeyQuery::Fetch(None);
							let r = col.get_at_value_index(key_query, address, log)?;
							return Ok(r.map(|r| (key, r.1)))
						} else {
							// forward end
							if self.exit(direction) {
								break
							}
						}
					},
					LastIndex::Descend(child_ix) => {
						if let Some(child) =
							col.with_locked(|btree| state.1.fetch_child(child_ix, btree, log))?
						{
							self.enter(child_ix, child, direction);
						} else {
							if self.exit(direction) {
								break
							}
						}
					},
					_ => unreachable!(),
				}
			} else {
				break
			}
		}

		Ok(None)
	}

	pub fn seek(
		&mut self,
		key: &[u8],
		btree: &mut BTree,
		col: &BTreeTable,
		log: &impl LogQuery,
		seek_to: SeekTo,
		direction: IterDirection,
	) -> Result<()> {
		self.state.clear();
		self.fetch_root = false;
		col.with_locked(|b| {
			let root = BTree::fetch_root(btree.root_index.unwrap_or(NULL_ADDRESS), b, log)?;
			Node::seek(root, key, b, log, btree.depth, &mut self.state, seek_to, direction)
		})
	}

	pub fn seek_to_last(
		&mut self,
		btree: &mut BTree,
		col: &BTreeTable,
		log: &impl LogQuery,
	) -> Result<()> {
		self.state.clear();
		self.fetch_root = false;
		col.with_locked(|b| {
			let root = BTree::fetch_root(btree.root_index.unwrap_or(NULL_ADDRESS), b, log)?;
			self.state.push((LastIndex::End, root));
			Ok(())
		})
	}
}
