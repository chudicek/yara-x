use std::mem;

use super::instr::{Instr, InstrParser};
use crate::re::{Action, CodeLoc, DEFAULT_SCAN_LIMIT};

/// Represents a [Pike's VM](https://swtch.com/~rsc/regexp/regexp2.html) that
/// executes VM code produced by the [compiler][`crate::re::compiler::Compiler`].
pub(crate) struct PikeVM<'r> {
    /// The code for the VM. Produced by [`crate::re::compiler::Compiler`].
    code: &'r [u8],
    /// The list of currently active threads. Each item in this list is a
    /// position within the VM code, pointing to some VM instruction. Each item
    /// in the list is unique, the VM guarantees that there aren't two active
    /// threads at the same VM instruction.
    threads: Vec<usize>,
    /// The list of threads that will become the active threads when the next
    /// byte is read from the input.
    next_threads: Vec<usize>,
    /// Maximum number of bytes to scan. The VM will abort after ingesting
    /// this number of bytes from the input.
    scan_limit: usize,
    /// State for the [`epsilon_closure`] function.
    cache: EpsilonClosureState,
}

impl<'r> PikeVM<'r> {
    /// Creates a new [`PikeVM`].
    pub fn new(code: &'r [u8]) -> Self {
        Self {
            code,
            threads: Vec::new(),
            next_threads: Vec::new(),
            cache: EpsilonClosureState::new(),
            scan_limit: DEFAULT_SCAN_LIMIT,
        }
    }

    /// Specifies the maximum number of bytes that will be scanned by the
    /// VM before aborting.
    ///
    /// This sets a limit on the number of bytes that the VM will read from the
    /// input while trying find a match. Without a limit, the VM will can incur
    /// in excessive execution time for regular expressions that are unbounded,
    /// like `foo.*bar`. For inputs that starts with `foo`, this regexp will
    /// try to scan the whole input, and that would take a long time if the
    /// input is excessively large.
    ///
    /// The default limit is 4096 bytes.
    pub fn scan_limit(mut self, limit: usize) -> Self {
        self.scan_limit = limit;
        self
    }

    /// Executes VM code starting at the `start` location and calls `f` for
    /// each match found. Input bytes are read from the `fwd_input` iterator
    /// until no more bytes are available or the scan limit is reached. When
    /// a match is found `f` is called with the number of bytes that matched.
    /// The number of matching bytes can be zero, as some regexps can match
    /// a zero-length string.
    ///
    /// The `f` function must return either [`Action::Continue`] or
    /// [`Action::Stop`], the former will cause the VM to keep trying to find
    /// longer matches, while the latter will stop the scanning.
    ///
    /// `bck_input` is an iterator that returns the bytes that are before
    /// the starting point of `fwd_input`, in reverse order. For instance,
    /// suppose we have the string `a b c e f g h i`, and `fwd_input` starts
    /// at the `f` character and returns `f`, `g`, `h` and `i` in that order.
    /// In such case `bck_input` will return `e`, `c`, `b` and `a`.
    ///
    /// ```text
    ///       a  b  c  e  f   g   h   i
    ///                   |
    ///      <- bck_input | fwd_input ->
    /// ```
    ///
    /// The purpose of `bck_input` is allowing the function to access the bytes
    /// that appear right before the start of `fwd_input` for matching some
    /// look-around assertions that need information about the surrounding
    /// bytes.
    pub(crate) fn try_match<'a, C, F, B>(
        &mut self,
        start: C,
        mut fwd_input: F,
        mut bck_input: B,
        mut f: impl FnMut(usize) -> Action,
    ) where
        C: CodeLoc,
        F: Iterator<Item = &'a u8>,
        B: Iterator<Item = &'a u8>,
    {
        let step = 1;
        let mut current_pos = 0;
        let mut curr_byte = fwd_input.next();

        // Make sure that the list of threads is empty when this function is
        // called.
        debug_assert!(self.threads.is_empty());

        epsilon_closure(
            self.code,
            start,
            curr_byte,
            bck_input.next(),
            &mut self.cache,
            &mut self.threads,
        );

        while !self.threads.is_empty() {
            let next_byte = fwd_input.next();

            for ip in self.threads.iter() {
                let (instr, size) =
                    InstrParser::decode_instr(&self.code[*ip..]);

                let is_match = match instr {
                    Instr::AnyByte => curr_byte.is_some(),
                    Instr::Byte(byte) => {
                        matches!(curr_byte, Some(b) if *b == byte)
                    }
                    Instr::MaskedByte { byte, mask } => {
                        matches!(curr_byte, Some(b) if *b & mask == byte)
                    }
                    Instr::CaseInsensitiveChar(byte) => {
                        matches!(curr_byte, Some(b) if b.to_ascii_lowercase() == byte)
                    }
                    Instr::ClassBitmap(class) => {
                        matches!(curr_byte, Some(b) if class.contains(*b))
                    }
                    Instr::ClassRanges(class) => {
                        matches!(curr_byte, Some(b) if class.contains(*b))
                    }
                    Instr::Match => match f(current_pos) {
                        Action::Stop => break,
                        Action::Continue => false,
                    },
                    Instr::Eoi => {
                        // TODO: is this correct?
                        break;
                    }
                    _ => unreachable!(),
                };

                if is_match {
                    epsilon_closure(
                        self.code,
                        C::from(*ip + size),
                        next_byte,
                        curr_byte,
                        &mut self.cache,
                        &mut self.next_threads,
                    );
                }
            }

            curr_byte = next_byte;
            current_pos += step;

            mem::swap(&mut self.threads, &mut self.next_threads);
            self.next_threads.clear();

            if current_pos >= self.scan_limit {
                self.threads.clear();
                break;
            }
        }
    }
}

/// Structure used by the [`epsilon_closure`] function for maintaining
/// its state during the computation of an epsilon closure. See the
/// documentation of [`epsilon_closure`] for details.
pub struct EpsilonClosureState {
    threads: Vec<usize>,
    executed_splits: Vec<usize>,
}

impl EpsilonClosureState {
    pub fn new() -> Self {
        Self { threads: Vec::new(), executed_splits: Vec::new() }
    }
}

/// Computes the epsilon closure derived from executing the code starting at
/// a given position.
///
/// In a NFA, the epsilon closure of some state `S`, is the set containing all
/// the states that can be reached from `S` by following epsilon transitions
/// (i.e: transitions that don't consume any input symbol). The Pike's VM code
/// produced for a regexp is simply another way of representing a NFA where
/// each instruction is a state. The NFA jumps from one state to the other by
/// following the instruction flow. Instructions like `jump` and `split`, which
/// jump from one state to another (or others) unconditionally, without
/// consuming a byte from the input, are epsilon transitions in this context.
///
/// This function starts at the instruction in the `start` location, and from
/// there explore all the possible transitions that don't depend on the next
/// value from the input. When some instruction that depends on the next
/// input is found (a non-epsilon transition) the location of that instruction
/// is added to the closure.
///
/// This function expects a mutable reference to a [`EpsilonClosureState`],
/// which is the structure used for keeping track of the current state while
/// computing the epsilon closure. Instead of creating a new instance of
/// [`EpsilonClosureState`] on each call to [`epsilon_closure`], the same
/// instance should be reused in order to prevent unnecessary allocations.
/// The function guarantees that the state is empty before returning, and
/// therefore it can be re-used safely.
#[inline(always)]
pub(crate) fn epsilon_closure<C: CodeLoc>(
    code: &[u8],
    start: C,
    curr_byte: Option<&u8>,
    prev_byte: Option<&u8>,
    state: &mut EpsilonClosureState,
    closure: &mut Vec<usize>,
) {
    state.threads.push(start.location());
    state.executed_splits.clear();

    while let Some(ip) = state.threads.pop() {
        let (instr, size) = InstrParser::decode_instr(&code[ip..]);
        let next = ip + size;
        match instr {
            Instr::AnyByte
            | Instr::Byte(_)
            | Instr::MaskedByte { .. }
            | Instr::CaseInsensitiveChar(_)
            | Instr::ClassBitmap(_)
            | Instr::ClassRanges(_)
            | Instr::Match => {
                if !closure.contains(&ip) {
                    closure.push(ip);
                }
            }
            Instr::SplitA(offset) => {
                // TODO: here we are relying on `contains` which is O(n), this
                // can be improved by using a set. We can even remove
                // `executed_splits` and rely on `closure`, which must be
                // a set that maintains the insertion order.
                if !state.executed_splits.contains(&ip) {
                    state.executed_splits.push(ip);
                    state
                        .threads
                        .push((ip as i64 + offset as i64).try_into().unwrap());
                    state.threads.push(next);
                }
            }
            Instr::SplitB(offset) => {
                if !state.executed_splits.contains(&ip) {
                    state.executed_splits.push(ip);
                    state.threads.push(next);
                    state
                        .threads
                        .push((ip as i64 + offset as i64).try_into().unwrap());
                }
            }
            Instr::SplitN(split) => {
                if !state.executed_splits.contains(&ip) {
                    state.executed_splits.push(ip);
                    for offset in split.offsets().rev() {
                        state.threads.push(
                            (ip as i64 + offset as i64).try_into().unwrap(),
                        );
                    }
                }
            }
            Instr::Jump(offset) => {
                state
                    .threads
                    .push((ip as i64 + offset as i64).try_into().unwrap());
            }
            Instr::Start => {
                if start.backwards() {
                    if curr_byte.is_none() {
                        state.threads.push(next);
                    }
                } else if prev_byte.is_none() {
                    state.threads.push(next);
                }
            }
            Instr::End => {
                if start.backwards() {
                    if prev_byte.is_none() {
                        state.threads.push(next);
                    }
                } else if curr_byte.is_none() {
                    state.threads.push(next);
                }
            }
            Instr::WordBoundary | Instr::WordBoundaryNeg => {
                let mut is_match = match (prev_byte, curr_byte) {
                    (Some(p), Some(c)) => {
                        p.is_ascii_alphanumeric() != c.is_ascii_alphanumeric()
                    }
                    (None, Some(b)) | (Some(b), None) => {
                        b.is_ascii_alphanumeric()
                    }
                    _ => false,
                };

                if matches!(instr, Instr::WordBoundaryNeg) {
                    is_match = !is_match;
                }

                if is_match {
                    state.threads.push(next)
                }
            }
            Instr::Eoi => {}
        }
    }
}