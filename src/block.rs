use std::cmp::Ordering;

use std::rc::Rc;

use options::Options;
use types::LdbIterator;

use integer_encoding::FixedInt;
use integer_encoding::VarInt;

pub type BlockContents = Vec<u8>;

/// A block is a list of ENTRIES followed by a list of RESTARTS, terminated by a fixed u32
/// N_RESTARTS.
///
/// An ENTRY consists of three varints, SHARED, NON_SHARED, VALSIZE, a KEY and a VALUE.
///
/// SHARED denotes how many bytes the entry's key shares with the previous one.
///
/// NON_SHARED is the size of the key minus SHARED.
///
/// VALSIZE is the size of the value.
///
/// KEY and VALUE are byte strings; the length of KEY is NON_SHARED.
///
/// A RESTART is a fixed u32 pointing to the beginning of an ENTRY.
///
/// N_RESTARTS contains the number of restarts.
#[derive(Clone)]
pub struct Block {
    block: Rc<BlockContents>,
    opt: Options,
}

impl Block {
    /// Return an iterator over this block.
    /// Note that the iterator isn't bound to the block's lifetime; the iterator uses the same
    /// refcounted block contents as this block. (meaning also that if the iterator isn't released,
    /// the memory occupied by the block isn't, either)
    pub fn iter(&self) -> BlockIter {
        let restarts = u32::decode_fixed(&self.block[self.block.len() - 4..]);
        let restart_offset = self.block.len() - 4 - 4 * restarts as usize;

        BlockIter {
            block: self.block.clone(),
            opt: self.opt.clone(),

            offset: 0,
            restarts_off: restart_offset,
            current_entry_offset: 0,
            current_restart_ix: 0,

            key: Vec::new(),
            val_offset: 0,
        }
    }

    pub fn contents(&self) -> Rc<BlockContents> {
        self.block.clone()
    }

    pub fn new(opt: Options, contents: BlockContents) -> Block {
        assert!(contents.len() > 4);
        Block {
            block: Rc::new(contents),
            opt: opt,
        }
    }
}

pub struct BlockIter {
    /// The underlying block contents.
    /// TODO: Maybe (probably...) this needs an Arc.
    block: Rc<BlockContents>,
    opt: Options,
    /// offset of restarts area within the block.
    restarts_off: usize,

    /// start of next entry to be parsed.
    offset: usize,
    /// offset of the current entry.
    current_entry_offset: usize,
    /// index of the most recent restart.
    current_restart_ix: usize,

    /// We assemble the key from two parts usually, so we keep the current full key here.
    key: Vec<u8>,
    /// Offset of the current value within the block.
    val_offset: usize,
}

impl BlockIter {
    /// Return the number of restarts in this block.
    fn number_restarts(&self) -> usize {
        u32::decode_fixed(&self.block[self.block.len() - 4..]) as usize
    }

    /// Seek to restart point `ix`. After the seek, current() will return the entry at that restart
    /// point.
    fn seek_to_restart_point(&mut self, ix: usize) {
        let off = self.get_restart_point(ix);

        self.offset = off;
        self.current_entry_offset = off;
        self.current_restart_ix = ix;
        // advances self.offset to point to the next entry
        let (shared, non_shared, _, head_len) = self.parse_entry_and_advance();

        assert_eq!(shared, 0);

        self.assemble_key(off + head_len, shared, non_shared);
    }

    /// Return the offset that restart `ix` points to.
    fn get_restart_point(&self, ix: usize) -> usize {
        let restart = self.restarts_off + 4 * ix;
        u32::decode_fixed(&self.block[restart..restart + 4]) as usize
    }

    /// The layout of an entry is
    /// [SHARED varint, NON_SHARED varint, VALSIZE varint, KEY (NON_SHARED bytes),
    ///  VALUE (VALSIZE bytes)].
    ///
    /// Returns SHARED, NON_SHARED, VALSIZE and [length of length spec] from the current position,
    /// where 'length spec' is the length of the three values in the entry header, as described
    /// above.
    /// Advances self.offset to the beginning of the next entry.
    fn parse_entry_and_advance(&mut self) -> (usize, usize, usize, usize) {
        let mut i = 0;
        let (shared, sharedlen) = usize::decode_var(&self.block[self.offset..]);
        i += sharedlen;

        let (non_shared, non_sharedlen) = usize::decode_var(&self.block[self.offset + i..]);
        i += non_sharedlen;

        let (valsize, valsizelen) = usize::decode_var(&self.block[self.offset + i..]);
        i += valsizelen;

        self.val_offset = self.offset + i + non_shared;
        self.offset = self.offset + i + non_shared + valsize;

        (shared, non_shared, valsize, i)
    }

    /// Assemble the current key from shared and non-shared parts (an entry usually contains only
    /// the part of the key that is different from the previous key).
    ///
    /// `off` is the offset of the key string within the whole block (self.current_entry_offset
    /// + entry header length); `shared` and `non_shared` are the lengths of the shared
    /// respectively non-shared parts of the key.
    /// Only self.key is mutated.
    fn assemble_key(&mut self, off: usize, shared: usize, non_shared: usize) {
        self.key.resize(shared, 0);
        self.key.extend_from_slice(&self.block[off..off + non_shared]);
    }

    pub fn seek_to_last(&mut self) {
        if self.number_restarts() > 0 {
            let num_restarts = self.number_restarts();
            self.seek_to_restart_point(num_restarts - 1);
        } else {
            self.reset();
        }

        while let Some((_, _)) = self.next() {
        }
    }
}

impl Iterator for BlockIter {
    type Item = (Vec<u8>, Vec<u8>);

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset >= self.restarts_off {
            self.offset = self.restarts_off;
            // current_entry_offset is left at the offset of the last entry
            return None;
        } else {
            self.current_entry_offset = self.offset;
        }

        let current_off = self.current_entry_offset;

        let (shared, non_shared, valsize, entry_head_len) = self.parse_entry_and_advance();
        self.assemble_key(current_off + entry_head_len, shared, non_shared);

        // Adjust current_restart_ix
        let num_restarts = self.number_restarts();
        while self.current_restart_ix + 1 < num_restarts &&
              self.get_restart_point(self.current_restart_ix + 1) < self.current_entry_offset {
            self.current_restart_ix += 1;
        }

        Some((self.key.clone(), Vec::from(&self.block[self.val_offset..self.val_offset + valsize])))
    }
}

impl LdbIterator for BlockIter {
    fn reset(&mut self) {
        self.offset = 0;
        self.val_offset = 0;
        self.current_restart_ix = 0;
        self.key.clear();
    }

    fn prev(&mut self) -> Option<Self::Item> {
        // as in the original implementation -- seek to last restart point, then look for key
        let orig_offset = self.current_entry_offset;

        // At the beginning, can't go further back
        if orig_offset == 0 {
            self.reset();
            return None;
        }

        while self.get_restart_point(self.current_restart_ix) >= orig_offset {
            // todo: double check this
            if self.current_restart_ix == 0 {
                self.offset = self.restarts_off;
                self.current_restart_ix = self.number_restarts();
                break;
            }
            self.current_restart_ix -= 1;
        }

        self.offset = self.get_restart_point(self.current_restart_ix);
        assert!(self.offset < orig_offset);

        let mut result;

        // Stop if the next entry would be the original one (self.offset always points to the start
        // of the next entry)
        loop {
            result = self.next();

            if self.offset >= orig_offset {
                break;
            }
        }
        result
    }

    fn seek(&mut self, to: &[u8]) {
        self.reset();

        let mut left = 0;
        let mut right = if self.number_restarts() == 0 {
            0
        } else {
            self.number_restarts() - 1
        };

        // Do a binary search over the restart points.
        while left < right {
            let middle = (left + right + 1) / 2;
            self.seek_to_restart_point(middle);

            let c = self.opt.cmp.cmp(&self.key, to);

            if c == Ordering::Less {
                left = middle;
            } else {
                right = middle - 1;
            }
        }

        assert_eq!(left, right);
        self.current_restart_ix = left;
        self.offset = self.get_restart_point(left);

        // Linear search from here on
        while let Some((k, _)) = self.next() {
            if self.opt.cmp.cmp(k.as_slice(), to) >= Ordering::Equal {
                return;
            }
        }
    }

    fn valid(&self) -> bool {
        !self.key.is_empty() && self.val_offset > 0 && self.val_offset < self.restarts_off
    }

    fn current(&self) -> Option<Self::Item> {
        if self.valid() {
            Some((self.key.clone(), Vec::from(&self.block[self.val_offset..self.offset])))
        } else {
            None
        }
    }
}

pub struct BlockBuilder {
    opt: Options,
    buffer: Vec<u8>,
    restarts: Vec<u32>,

    last_key: Vec<u8>,
    counter: usize,
}

impl BlockBuilder {
    pub fn new(o: Options) -> BlockBuilder {
        let mut restarts = vec![0];
        restarts.reserve(1023);

        BlockBuilder {
            buffer: Vec::with_capacity(o.block_size),
            opt: o,
            restarts: restarts,
            last_key: Vec::new(),
            counter: 0,
        }
    }

    pub fn entries(&self) -> usize {
        self.counter
    }

    pub fn last_key<'a>(&'a self) -> &'a [u8] {
        &self.last_key
    }

    pub fn size_estimate(&self) -> usize {
        self.buffer.len() + self.restarts.len() * 4 + 4
    }

    pub fn reset(&mut self) {
        self.buffer.clear();
        self.restarts.clear();
        self.last_key.clear();
        self.counter = 0;
    }

    pub fn add(&mut self, key: &[u8], val: &[u8]) {
        assert!(self.counter <= self.opt.block_restart_interval);
        assert!(self.buffer.is_empty() ||
                self.opt.cmp.cmp(self.last_key.as_slice(), key) == Ordering::Less);

        let mut shared = 0;

        if self.counter < self.opt.block_restart_interval {
            let smallest = if self.last_key.len() < key.len() {
                self.last_key.len()
            } else {
                key.len()
            };

            while shared < smallest && self.last_key[shared] == key[shared] {
                shared += 1;
            }
        } else {
            self.restarts.push(self.buffer.len() as u32);
            self.last_key.resize(0, 0);
            self.counter = 0;
        }

        let non_shared = key.len() - shared;

        let mut buf = [0 as u8; 4];

        let mut sz = shared.encode_var(&mut buf[..]);
        self.buffer.extend_from_slice(&buf[0..sz]);
        sz = non_shared.encode_var(&mut buf[..]);
        self.buffer.extend_from_slice(&buf[0..sz]);
        sz = val.len().encode_var(&mut buf[..]);
        self.buffer.extend_from_slice(&buf[0..sz]);

        self.buffer.extend_from_slice(&key[shared..]);
        self.buffer.extend_from_slice(val);

        // Update key
        self.last_key.resize(shared, 0);
        self.last_key.extend_from_slice(&key[shared..]);

        self.counter += 1;
    }

    pub fn finish(mut self) -> BlockContents {
        // 1. Append RESTARTS
        let mut i = self.buffer.len();
        self.buffer.resize(i + self.restarts.len() * 4 + 4, 0);

        for r in self.restarts.iter() {
            r.encode_fixed(&mut self.buffer[i..i + 4]);
            i += 4;
        }

        // 2. Append N_RESTARTS
        (self.restarts.len() as u32).encode_fixed(&mut self.buffer[i..i + 4]);

        // done
        self.buffer
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use types::*;
    use options::*;

    fn get_data() -> Vec<(&'static [u8], &'static [u8])> {
        vec![("key1".as_bytes(), "value1".as_bytes()),
             ("loooooooooooooooooooooooooooooooooongerkey1".as_bytes(), "shrtvl1".as_bytes()),
             ("medium length key 1".as_bytes(), "some value 2".as_bytes()),
             ("prefix_key1".as_bytes(), "value".as_bytes()),
             ("prefix_key2".as_bytes(), "value".as_bytes()),
             ("prefix_key3".as_bytes(), "value".as_bytes())]
    }

    #[test]
    fn test_block_builder() {
        let mut o = Options::default();
        o.block_restart_interval = 3;

        let mut builder = BlockBuilder::new(o);

        for &(k, v) in get_data().iter() {
            builder.add(k, v);
            assert!(builder.counter <= 3);
            assert_eq!(builder.last_key(), k);
        }

        let block = builder.finish();
        assert_eq!(block.len(), 149);
    }

    #[test]
    fn test_block_empty() {
        let mut o = Options::default();
        o.block_restart_interval = 16;
        let builder = BlockBuilder::new(o);

        let blockc = builder.finish();
        assert_eq!(blockc.len(), 8);
        assert_eq!(blockc, vec![0, 0, 0, 0, 1, 0, 0, 0]);

        let block = Block::new(Options::default(), blockc);

        for _ in block.iter() {
            panic!("expected 0 iterations");
        }
    }

    #[test]
    fn test_block_build_iterate() {
        let data = get_data();
        let mut builder = BlockBuilder::new(Options::default());

        for &(k, v) in data.iter() {
            builder.add(k, v);
        }

        let block_contents = builder.finish();
        let block = Block::new(Options::default(), block_contents).iter();
        let mut i = 0;

        assert!(!block.valid());

        for (k, v) in block {
            assert_eq!(&k[..], data[i].0);
            assert_eq!(v, data[i].1);
            i += 1;
        }
        assert_eq!(i, data.len());
    }

    #[test]
    fn test_block_iterate_reverse() {
        let mut o = Options::default();
        o.block_restart_interval = 3;
        let data = get_data();
        let mut builder = BlockBuilder::new(o.clone());

        for &(k, v) in data.iter() {
            builder.add(k, v);
        }

        let block_contents = builder.finish();
        let mut block = Block::new(o.clone(), block_contents).iter();

        // assert!(!block.valid());
        // assert_eq!(block.next(),
        // Some(("key1".as_bytes().to_vec(), "value1".as_bytes().to_vec())));
        // assert!(block.valid());
        // block.next();
        // assert!(block.valid());
        // block.prev();
        // assert!(block.valid());
        // assert_eq!(block.current(),
        // Some(("key1".as_bytes().to_vec(), "value1".as_bytes().to_vec())));
        // block.prev();
        // assert!(!block.valid());
        //
        // Verify that prev() from the last entry goes to the prev-to-last entry
        // (essentially, that next() returning None doesn't advance anything)
        while let Some(_) = block.next() {
        }

        block.prev();
        assert!(block.valid());
        assert_eq!(block.current(),
                   Some(("prefix_key2".as_bytes().to_vec(), "value".as_bytes().to_vec())));
    }

    #[test]
    fn test_block_seek() {
        let mut o = Options::default();
        o.block_restart_interval = 3;

        let data = get_data();
        let mut builder = BlockBuilder::new(o.clone());

        for &(k, v) in data.iter() {
            builder.add(k, v);
        }

        let block_contents = builder.finish();

        let mut block = Block::new(o.clone(), block_contents).iter();

        block.seek(&"prefix_key2".as_bytes());
        assert!(block.valid());
        assert_eq!(block.current(),
                   Some(("prefix_key2".as_bytes().to_vec(), "value".as_bytes().to_vec())));

        block.seek(&"prefix_key0".as_bytes());
        assert!(block.valid());
        assert_eq!(block.current(),
                   Some(("prefix_key1".as_bytes().to_vec(), "value".as_bytes().to_vec())));

        block.seek(&"key1".as_bytes());
        assert!(block.valid());
        assert_eq!(block.current(),
                   Some(("key1".as_bytes().to_vec(), "value1".as_bytes().to_vec())));

        block.seek(&"prefix_key3".as_bytes());
        assert!(block.valid());
        assert_eq!(block.current(),
                   Some(("prefix_key3".as_bytes().to_vec(), "value".as_bytes().to_vec())));

        block.seek(&"prefix_key8".as_bytes());
        assert!(block.valid());
        assert_eq!(block.current(),
                   Some(("prefix_key3".as_bytes().to_vec(), "value".as_bytes().to_vec())));
    }

    #[test]
    fn test_block_seek_to_last() {
        let mut o = Options::default();

        // Test with different number of restarts
        for block_restart_interval in vec![2, 6, 10] {
            o.block_restart_interval = block_restart_interval;

            let data = get_data();
            let mut builder = BlockBuilder::new(o.clone());

            for &(k, v) in data.iter() {
                builder.add(k, v);
            }

            let block_contents = builder.finish();

            let mut block = Block::new(o.clone(), block_contents).iter();

            block.seek_to_last();
            assert!(block.valid());
            assert_eq!(block.current(),
                       Some(("prefix_key3".as_bytes().to_vec(), "value".as_bytes().to_vec())));
        }
    }
}
