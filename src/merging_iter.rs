use cmp::Cmp;
use options::Options;
use types::{current_key_val, LdbIterator};

use std::cmp::Ordering;
use std::sync::Arc;

// Warning: This module is kinda messy. The original implementation is
// not that much better though :-)
//
// Issues: 1) prev() may not work correctly at the beginning of a merging
// iterator.

#[derive(PartialEq)]
enum SL {
    Smallest,
    Largest,
}

#[derive(PartialEq)]
enum Direction {
    Fwd,
    Rvrs,
}

pub struct MergingIter {
    iters: Vec<Box<LdbIterator>>,
    current: Option<usize>,
    direction: Direction,
    cmp: Arc<Box<Cmp>>,
}

impl MergingIter {
    /// Construct a new merging iterator.
    pub fn new(opt: Options, iters: Vec<Box<LdbIterator>>) -> MergingIter {
        let mi = MergingIter {
            iters: iters,
            current: None,
            direction: Direction::Fwd,
            cmp: opt.cmp,
        };
        mi
    }

    fn init(&mut self) {
        for i in 0..self.iters.len() {
            self.iters[i].reset();
            self.iters[i].advance();
            assert!(self.iters[i].valid());
        }
        self.find_smallest();
    }

    /// Adjusts the direction of the iterator depending on whether the last
    /// call was next() or prev(). This basically sets all iterators to one
    /// entry after (Fwd) or one entry before (Rvrs) the current() entry.
    fn update_direction(&mut self, d: Direction) {
        if let Some((key, _)) = current_key_val(self) {
            if let Some(current) = self.current {
                match d {
                    Direction::Fwd if self.direction == Direction::Rvrs => {
                        self.direction = Direction::Fwd;
                        for i in 0..self.iters.len() {
                            if i != current {
                                self.iters[i].seek(&key);
                                // This doesn't work if two iterators are returning the exact same
                                // keys. However, in reality, two entries will always have differing
                                // sequence numbers.
                                if let Some((current_key, _)) = current_key_val(self.iters[i].as_ref()) {
                                    if self.cmp.cmp(&current_key, &key) == Ordering::Equal {
                                        self.iters[i].advance();
                                    }
                                }
                            }
                        }
                    }
                    Direction::Rvrs if self.direction == Direction::Fwd => {
                        self.direction = Direction::Rvrs;
                        for i in 0..self.iters.len() {
                            if i != current {
                                self.iters[i].seek(&key);
                                if self.iters[i].valid() {
                                    self.iters[i].prev();
                                } else {
                                    // seek to last.
                                    while self.iters[i].advance() {}
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn find_smallest(&mut self) {
        self.find(SL::Smallest)
    }
    fn find_largest(&mut self) {
        self.find(SL::Largest)
    }

    fn find(&mut self, direction: SL) {
        assert!(self.iters.len() > 0);

        let ord;

        if direction == SL::Smallest {
            ord = Ordering::Less;
        } else {
            ord = Ordering::Greater;
        }

        let mut next_ix = 0;

        for i in 1..self.iters.len() {
            if let Some((current, _)) = current_key_val(self.iters[i].as_ref()) {
                if let Some((smallest, _)) = current_key_val(self.iters[next_ix].as_ref()) {
                    if self.cmp.cmp(&current, &smallest) == ord {
                        next_ix = i;
                    }
                } else {
                    next_ix = i;
                }
            }
        }

        self.current = Some(next_ix);
    }
}

impl LdbIterator for MergingIter {
    fn advance(&mut self) -> bool {
        if let Some(current) = self.current {
            self.update_direction(Direction::Fwd);
            if !self.iters[current].advance() {
                // Take this iterator out of rotation; this will return None
                // for every call to current() and thus it will be ignored
                // from here on.
                self.iters[current].reset();
            }
            self.find_smallest();
        } else {
            self.init();
        }
        self.valid()
    }
    fn valid(&self) -> bool {
        if let Some(ix) = self.current {
            // TODO: second clause is unnecessary, because first asserts that at least one iterator
            // is valid.
            self.iters[ix].valid() && self.iters.iter().any(|it| it.valid())
        } else {
            false
        }
    }
    fn seek(&mut self, key: &[u8]) {
        for i in 0..self.iters.len() {
            self.iters[i].seek(key);
        }
        self.find_smallest();
    }
    fn reset(&mut self) {
        for i in 0..self.iters.len() {
            self.iters[i].reset();
        }
    }
    fn current(&self, key: &mut Vec<u8>, val: &mut Vec<u8>) -> bool {
        if let Some(ix) = self.current {
            self.iters[ix].current(key, val)
        } else {
            false
        }
    }
    fn prev(&mut self) -> bool {
        if let Some(current) = self.current {
            if self.iters[current].valid() {
                self.update_direction(Direction::Rvrs);
                self.iters[current].prev();
                self.find_largest();
                true
            } else {
                false
            }
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use options::Options;
    use test_util::{LdbIteratorIter, TestLdbIter};
    use types::{current_key_val, LdbIterator};
    use skipmap::tests;

    #[test]
    fn test_merging_one() {
        let skm = tests::make_skipmap();
        let iter = skm.iter();
        let mut iter2 = skm.iter();

        let mut miter = MergingIter::new(Options::default(), vec![Box::new(iter)]);

        loop {
            if let Some((k, v)) = miter.next() {
                if let Some((k2, v2)) = iter2.next() {
                    assert_eq!(k, k2);
                    assert_eq!(v, v2);
                } else {
                    panic!("Expected element from iter2");
                }
            } else {
                break;
            }
        }
    }

    #[test]
    fn test_merging_two() {
        let skm = tests::make_skipmap();
        let iter = skm.iter();
        let iter2 = skm.iter();

        let mut miter = MergingIter::new(Options::default(), vec![Box::new(iter), Box::new(iter2)]);

        loop {
            if let Some((k, v)) = miter.next() {
                if let Some((k2, v2)) = miter.next() {
                    assert_eq!(k, k2);
                    assert_eq!(v, v2);
                } else {
                    panic!("Odd number of elements");
                }
            } else {
                break;
            }
        }
    }

    #[test]
    fn test_merging_fwd_bckwd() {
        let val = "def".as_bytes();
        let iter = TestLdbIter::new(vec![(b("aba"), val), (b("abc"), val), (b("abe"), val)]);
        let iter2 = TestLdbIter::new(vec![(b("abb"), val), (b("abd"), val)]);

        let mut miter = MergingIter::new(Options::default(), vec![Box::new(iter), Box::new(iter2)]);

        // miter should return the following sequence: [aba, abb, abc, abd, abe]

        // -> aba
        let first = miter.next();
        // -> abb
        let second = miter.next();
        // -> abc
        let third = miter.next();
        println!("{:?} {:?} {:?}", first, second, third);

        assert!(first != third);
        // abb <-
        assert!(miter.prev());
        assert_eq!(second, current_key_val(&miter));
        // aba <-
        assert!(miter.prev());
        assert_eq!(first, current_key_val(&miter));
        // -> abb
        assert!(miter.advance());
        assert_eq!(second, current_key_val(&miter));
        // -> abc
        assert!(miter.advance());
        assert_eq!(third, current_key_val(&miter));
        // -> abd
        assert!(miter.advance());
        assert_eq!(Some((b("abd").to_vec(), val.to_vec())), current_key_val(&miter));
    }

    fn b(s: &'static str) -> &'static [u8] {
        s.as_bytes()
    }

    #[test]
    fn test_merging_real() {
        let val = "def".as_bytes();

        let it1 = TestLdbIter::new(vec![(&b("aba"), val), (&b("abc"), val), (&b("abe"), val)]);
        let it2 = TestLdbIter::new(vec![(&b("abb"), val), (&b("abd"), val)]);
        let expected = vec![b("aba"), b("abb"), b("abc"), b("abd"), b("abe")];

        let mut iter = MergingIter::new(Options::default(), vec![Box::new(it1), Box::new(it2)]);

        let mut i = 0;
        for (k, _) in LdbIteratorIter::wrap(&mut iter) {
            assert_eq!(k, expected[i]);
            i += 1;
        }

    }

    #[test]
    fn test_merging_seek_reset() {
        let val = "def".as_bytes();

        let it1 = TestLdbIter::new(vec![(b("aba"), val), (b("abc"), val), (b("abe"), val)]);
        let it2 = TestLdbIter::new(vec![(b("abb"), val), (b("abd"), val)]);

        let mut iter = MergingIter::new(Options::default(), vec![Box::new(it1), Box::new(it2)]);

        assert!(!iter.valid());
        iter.advance();
        assert!(iter.valid());
        assert!(current_key_val(&iter).is_some());

        iter.seek("abc".as_bytes());
        assert_eq!(current_key_val(&iter),
                   Some((b("abc").to_vec(), val.to_vec())));
        iter.seek("ab0".as_bytes());
        assert_eq!(current_key_val(&iter),
                   Some((b("aba").to_vec(), val.to_vec())));
        iter.seek("abx".as_bytes());
        assert_eq!(current_key_val(&iter), None);

        iter.reset();
        assert!(!iter.valid());
        iter.next();
        assert_eq!(current_key_val(&iter),
                   Some((b("aba").to_vec(), val.to_vec())));
    }

    //#[test]
    fn test_merging_fwd_bckwd_2() {
        let val = "def".as_bytes();

        let it1 = TestLdbIter::new(vec![(b("aba"), val), (b("abc"), val), (b("abe"), val)]);
        let it2 = TestLdbIter::new(vec![(b("abb"), val), (b("abd"), val)]);

        let mut iter = MergingIter::new(Options::default(), vec![Box::new(it1), Box::new(it2)]);

        iter.next();
        iter.next();
        loop {
            let a = iter.next();

            if let None = a {
                break;
            }
            let b = iter.prev();
            let c = iter.next();
            iter.next();

            println!("{:?}", (a, b, c));
        }
    }
}
