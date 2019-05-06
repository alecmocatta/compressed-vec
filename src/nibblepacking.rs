#![allow(unused)] // needed for dbg!() macro, but folks say this should not be needed

use crate::byteutils::*;

#[derive(Debug, PartialEq)]
pub enum NibblePackError {
    InputTooShort,
}

/// Packs a slice of u64 numbers that are increasing, using delta encoding.  That is, the delta between successive
/// elements is encoded, rather than the absolute numbers.  The first number is encoded as is.
///
/// ## Numbers must be increasing
/// This is currently only designed for the case where successive numbers are either the same or increasing
/// (such as Prometheus-style increasing histograms).  If a successive input is less than the previous input,
/// currently this method WILL CLIP and record the difference as 0.
pub fn pack_u64_delta(inputs: &[u64], out_buffer: &mut Vec<u8>) {
    let mut last = 0u64;
    let deltas = inputs.into_iter().map(|&n| {
        let delta = n.saturating_sub(last);
        last = n;
        delta
    });
    pack_u64(deltas, out_buffer)
}

/// Packs a stream of double-precision IEEE-754 / f64 numbers using XOR encoding.
/// The first f64 is written as is; after that, each successive f64 is XORed with the previous one and the xor
/// value is written, based on the premise that when changes are small so is the XORed value.
/// Stream must have at least one value, otherwise InputTooShort is returned
pub fn pack_f64_xor<I: Iterator<Item = f64>>(mut stream: I, out_buffer: &mut Vec<u8>) -> Result<(), NibblePackError> {
    let mut last: u64 = match stream.next() {
        Some(num) => {
            let num_bits = num.to_bits();
            direct_write_uint_le(out_buffer, num_bits, 8);
            num_bits
        },
        None      => return Err(NibblePackError::InputTooShort)
    };
    pack_u64(stream.map(|f| {
        let f_bits = f.to_bits();
        let delta = last ^ f_bits;
        last = f_bits;
        delta
    }), out_buffer);
    Ok(())
}


///
/// Packs a stream of plain u64 numbers using NibblePacking.
///
/// This is especially powerful when combined with
/// other packers which can do for example delta or floating point XOR or other kinds of encoding which reduces
/// the # of bits needed and produces many zeroes.  This is why an Iterator is used for the API, as sources will
/// typically transform the incoming data by reducing the bits needed.
/// This method does no transformations to the input data.  You might want one of the other pack_* methods.
///
/// ```
/// # use compressed_vec::nibblepacking;
///     let inputs = [0u64, 1000, 1001, 1002, 1003, 2005, 2010, 3034, 4045, 5056, 6067, 7078];
///     let mut buf = Vec::with_capacity(1024);
///     nibblepacking::pack_u64(inputs.into_iter().cloned(), &mut buf);
/// ```
/// NOTE: The NibblePack algorithm always packs 8 u64's at a time.  If the length of the input stream is not
/// divisible by 8, extra 0 values pad the input.
// TODO: should this really be a function, or maybe a struct with more methods?
// TODO: also benchmark this vs just reading from a slice of u64's
#[inline]
pub fn pack_u64<I: Iterator<Item = u64>>(stream: I, out_buffer: &mut Vec<u8>) {
    let mut in_buffer = [0u64; 8];
    let mut bufindex = 0;
    // NOTE: using pointer math is actually NOT any faster!
    for num in stream {
        in_buffer[bufindex] = num;
        bufindex += 1;
        if bufindex >= 8 {
            // input buffer is full, encode!
            nibble_pack8(&in_buffer, out_buffer);
            bufindex = 0;
        }
    }
    // If buffer is partially filled, then encode the remainer
    if bufindex > 0 {
        while bufindex < 8 {
            in_buffer[bufindex] = 0;
            bufindex += 1;
        }
        nibble_pack8(&in_buffer, out_buffer);
    }
}

///
/// NibblePacking is an encoding technique for packing 8 u64's tightly into the same number of nibbles.
/// It can be combined with a prediction algorithm to efficiency encode floats and long values.
/// This is really an inner function; the intention is for the user to use one of the higher level pack* methods.
/// Please see http://github.com/filodb/FiloDB/doc/compression.md for more answers.
///
/// # Arguments
/// * `inputs` - ref to 8 u64 values to pack, could be the output of a predictor
/// * `out_buffer` - a vec to write the encoded output to.  Bytes are added at the end - vec is not cleared.
///
#[inline]
pub fn nibble_pack8(inputs: &[u64; 8], out_buffer: &mut Vec<u8>) {
    // Compute the nonzero bitmask.  TODO: use SIMD here
    let mut nonzero_mask = 0u8;
    for i in 0..8 {
        if inputs[i] != 0 {
            nonzero_mask |= 1 << i;
        }
    }
    out_buffer.push(nonzero_mask);

    // if no nonzero values, we're done!
    if nonzero_mask != 0 {
        // TODO: use SIMD here
        // otherwise, get min of leading and trailing zeros, encode it
        let min_leading_zeros = inputs.into_iter().map(|x| x.leading_zeros()).min().unwrap();
        let min_trailing_zeros = inputs.into_iter().map(|x| x.trailing_zeros()).min().unwrap();
        // Below impl seems to be equally fast, though it generates much more efficient code and SHOULD be much faster
        // let mut ored_bits = 0u64;
        // inputs.into_iter().for_each(|&x| ored_bits |= x);
        // let min_leading_zeros = ored_bits.leading_zeros();
        // let min_trailing_zeros = ored_bits.trailing_zeros();

        // Convert min leading/trailing to # nibbles.  Start packing!
        // NOTE: num_nibbles cannot be 0; that would imply every input was zero
        let trailing_nibbles = min_trailing_zeros / 4;
        let num_nibbles = 16 - (min_leading_zeros / 4) - trailing_nibbles;
        let nibble_word = (((num_nibbles - 1) << 4) | trailing_nibbles) as u8;
        out_buffer.push(nibble_word);

        if (num_nibbles % 2) == 0 {
            pack_to_even_nibbles(inputs, out_buffer, num_nibbles, trailing_nibbles);
        } else {
            pack_universal(inputs, out_buffer, num_nibbles, trailing_nibbles);
        }
    }
}

///
/// Inner function to pack the raw inputs to nibbles when # nibbles is even (always # bytes)
/// It's somehow really fast, perhaps because it is really simple.
///
/// # Arguments
/// * `trailing_zero_nibbles` - the min # of trailing zero nibbles across all inputs
/// * `num_nibbles` - the max # of nibbles having nonzero bits in all inputs
#[inline]
fn pack_to_even_nibbles(
    inputs: &[u64; 8],
    out_buffer: &mut Vec<u8>,
    num_nibbles: u32,
    trailing_zero_nibbles: u32,
) {
    // In the future, explore these optimizations: functions just for specific nibble widths
    let shift = trailing_zero_nibbles * 4;
    assert!(num_nibbles % 2 == 0);
    let num_bytes_each = (num_nibbles / 2) as usize;

    // for each nonzero input, shift and write out exact # of bytes
    inputs.into_iter().for_each(|&x| {
        if (x != 0) {
            direct_write_uint_le(out_buffer, x >> shift, num_bytes_each)
        }
    });
}

/// This code is inspired by bitpacking crate: https://github.com/tantivy-search/bitpacking/
/// but modified for the NibblePacking algorithm.  No macros, so slightly less efficient.
/// TODO: consider using macros like in bitpacking to achieve even more speed :D
#[inline]
fn pack_universal(
    inputs: &[u64; 8],
    out_buffer: &mut Vec<u8>,
    num_nibbles: u32,
    trailing_zero_nibbles: u32,
) {
    let trailing_shift = trailing_zero_nibbles * 4;
    let num_bits = num_nibbles * 4;
    let mut out_word = 0u64;
    let mut bit_cursor = 0;

    inputs.into_iter().for_each(|&x| {
        if (x != 0) {
            let remaining = 64 - bit_cursor;
            let shifted_input = x >> trailing_shift;

            // This is least significant portion of input
            out_word |= shifted_input << bit_cursor;

            // Write out current word if we've used up all 64 bits
            if remaining <= num_bits {
                direct_write_uint_le(out_buffer, out_word, 8); // Replace with faster write?
                if remaining < num_bits {
                    // Most significant portion left over from previous word
                    out_word = shifted_input >> (remaining as i32);
                } else {
                    out_word = 0; // reset for 64-bit input case
                }
            }

            bit_cursor = (bit_cursor + num_bits) % 64;
        }
    });

    // Write remainder word if there are any bits remaining, and only advance buffer right # of bytes
    if bit_cursor > 0 {
        direct_write_uint_le(out_buffer, out_word, ((bit_cursor + 7) / 8) as usize);
    }
}

///
/// A trait for processing data during unpacking.  Used to combine with predictors to create final output.
pub trait Sink {
    /// Called to reserve space for safely writing a number of items, perhaps in bulk.
    /// For example may be used to reserve space in a Vec to ensure individual process() methods can proceed quickly.
    fn reserve(&mut self, num_items: usize);

    /// The process method processes output, assuming space has been reserved using reserve()
    #[inline]
    fn process(&mut self, data: u64);

    /// Processes 8 items all having the same value.  Bulk process method for faster decoding.
    #[inline]
    fn process8(&mut self, data: u64);
}

/// A super-simple Sink which just appends to a Vec<u64>
#[derive(Debug)]
pub struct LongSink {
    vec: Vec<u64>,
}

const DEFAULT_CAPACITY: usize = 64;

impl LongSink {
    pub fn new() -> LongSink {
        LongSink { vec: Vec::with_capacity(DEFAULT_CAPACITY) }
    }

    pub fn clear(&mut self) {
        self.vec.clear()
    }
}

impl Sink for LongSink {
    #[inline]
    fn reserve(&mut self, num_items: usize) {
        self.vec.reserve(num_items)
    }

    #[inline]
    fn process(&mut self, data: u64) {
        unsafe { unchecked_write_u64_u64_le(&mut self.vec, data) }
    }

    #[inline]
    fn process8(&mut self, data: u64) {
        write8_u64_le(&mut self.vec, data)
    }
}

/// A Sink which accumulates delta-encoded NibblePacked data back into increasing u64 numbers
#[derive(Debug)]
pub struct DeltaSink {
    acc: u64,
    sink: LongSink,
}

impl DeltaSink {
    pub fn with_sink(inner_sink: LongSink) -> DeltaSink {
        DeltaSink { acc: 0, sink: inner_sink }
    }

    pub fn new() -> DeltaSink {
        DeltaSink::with_sink(LongSink::new())
    }

    /// Resets the state of the sink so it can be re-used for another unpack
    pub fn clear(&mut self) {
        self.acc = 0;
        self.sink.clear()
    }
}

impl Sink for DeltaSink {
    #[inline]
    fn reserve(&mut self, num_items: usize) {
        self.sink.reserve(num_items)
    }

    #[inline]
    fn process(&mut self, data: u64) {
        self.acc += data;
        self.sink.process(self.acc);
    }

    #[inline]
    fn process8(&mut self, data: u64) {
        self.acc += data;
        self.sink.process8(self.acc)
    }
}

/// A sink which uses simple successive XOR encoding to decode a NibblePacked floating point stream
/// encoded using [`pack_f64_xor`]: #method.pack_f64_xor
#[derive(Debug)]
pub struct DoubleXorSink {
    last: u64,
    vec: Vec<f64>,
}

impl DoubleXorSink {
    /// Creates a new DoubleXorSink with a vec which is owned by this struct.
    pub fn new(the_vec: Vec<f64>) -> DoubleXorSink {
        DoubleXorSink { last: 0, vec: the_vec }
    }

    fn reset(&mut self, init_value: u64) {
        self.vec.clear();
        self.vec.push(f64::from_bits(init_value));
        self.last = init_value;
    }
}

impl Sink for DoubleXorSink {
    #[inline]
    fn reserve(&mut self, num_items: usize) {
        self.vec.reserve(num_items)
    }

    // TODO: use optimized write methods in byteutils
    #[inline]
    fn process(&mut self, data: u64) {
        // XOR new piece of data with last, which yields original value
        let numbits = self.last ^ data;
        self.vec.push(f64::from_bits(numbits));
        self.last = numbits;
    }

    #[inline]
    fn process8(&mut self, data: u64) {
        // XOR new piece of data with last, which yields original value
        let numbits = self.last ^ data;
        self.last = numbits;
        for _ in 0..8 {
            self.vec.push(f64::from_bits(numbits));
        }
    }
}

///
/// A sink used for increasing histogram counters.  In one shot:
/// - Unpacks a delta-encoded NibblePack compressed Histogram
/// - Subtracts the values from lastHistValues, noting if the difference is not >= 0 (means counter reset)
/// - Packs the subtracted values
/// - Updates lastHistValues to the latest unpacked values so this sink can be used again
///
/// Meant to be used again and again to parse next histogram, thus the last_hist_deltas
/// state is reused to compute the next set of deltas.
/// If the new set of values is less than last_hist_deltas then the new set of values is
/// encoded instead of the diffs.
/// For more details, see the "2D Delta" section in [compression.md](doc/compression.md)
#[derive(Default)]
#[derive(Debug)]
pub struct DeltaDiffPackSink {
    value_dropped: bool,
    i: usize,
    last_hist_deltas: Vec<u64>,
    pack_array: [u64; 8],
    out_vec: Vec<u8>,
}

impl DeltaDiffPackSink {
    /// Creates new DeltaDiffPackSink, transferring ownership of out_vec
    pub fn new(num_buckets: usize, out_vec: Vec<u8>) -> DeltaDiffPackSink {
        let mut last_hist_deltas = Vec::<u64>::with_capacity(num_buckets);
        last_hist_deltas.resize(num_buckets, 0);
        DeltaDiffPackSink { last_hist_deltas, out_vec, ..Default::default() }
    }

    // Resets everythin, even the out_vec.  Probably should be used only for testing
    #[inline]
    pub fn reset(&mut self) {
        self.i = 0;
        self.value_dropped = false;
        for elem in self.last_hist_deltas.iter_mut() {
            *elem = 0;
        }
        self.out_vec.truncate(0);
    }

    /// Call this to finish packing the remainder of the deltas and reset for next go
    #[inline]
    pub fn finish(&mut self) {
        // TODO: move this to a pack_remainder function?
        if self.i != 0 {
            for j in self.i..8 {
                self.pack_array[j] = 0;
            }
            nibble_pack8(&self.pack_array, &mut self.out_vec);
        }
        self.i = 0;
        self.value_dropped = false;
    }
}

impl Sink for DeltaDiffPackSink {
    #[inline]
    fn reserve(&mut self, num_items: usize) {}

    #[inline]
    fn process(&mut self, data: u64) {
        if self.i < self.last_hist_deltas.len() {
            let last_value = self.last_hist_deltas[self.i];
            // If data dropped from last, write data instead of diff
            if data < last_value {
                self.value_dropped = true;
                self.pack_array[self.i % 8] = data;
            } else {
                self.pack_array[self.i % 8] = data - last_value;
            }
            self.last_hist_deltas[self.i] = data;
            self.i += 1;
            if (self.i % 8) == 0 {
                nibble_pack8(&self.pack_array, &mut self.out_vec);
            }
        }
    }

    #[inline]
    fn process8(&mut self, data: u64) {
        // Shortcut only if data==0: then just pack zeroes
        // Disable for now as we are not completely sure if this is legit
        // if data == 0 {
        //     assert!((self.i % 8) == 0);
        //     nibble_pack8(&[0; 8], &mut self.out_vec);
        // } else {
            for _ in 0..8 {
                self.process(data);
            }
        // }
    }
}

/// Unpacks num_values values from an encoded buffer, by calling nibble_unpack8 enough times.
/// The output.process() method is called numValues times rounded up to the next multiple of 8.
/// Returns "remainder" byteslice or unpacking error (say if one ran out of space)
///
/// # Arguments
/// * `inbuf` - NibblePacked compressed byte slice containing "remaining" bytes, starting with bitmask byte
/// * `output` - a Trait which processes each resulting u64
/// * `num_values` - the number of u64 values to decode
#[inline]
pub fn unpack<'a, Output: Sink>(
    encoded: &'a [u8],
    output: &mut Output,
    num_values: usize,
) -> Result<&'a [u8], NibblePackError> {
    let mut values_left = num_values as isize;
    let mut inbuf = encoded;
    while values_left > 0 {
        inbuf = nibble_unpack8(inbuf, output)?;
        values_left -= 8;
    }
    Ok(inbuf)
}

/// Unpacks a buffer encoded with [`pack_f64_xor`]: #method.pack_f64_xor
///
/// This wraps unpack() method with a read of the initial f64 value. InputTooShort error is returned
/// if the input does not have enough bytes given the number of values read.
/// NOTE: the sink is automatically cleared at the beginning.
///
/// ```
/// # use compressed_vec::nibblepacking;
/// # let encoded = [0xffu8; 16];
///     let mut out = Vec::<f64>::with_capacity(64);
///     let mut sink = nibblepacking::DoubleXorSink::new(out);
///     let res = nibblepacking::unpack_f64_xor(&encoded[..], &mut sink, 16);
/// ```
pub fn unpack_f64_xor<'a>(encoded: &'a [u8],
                          sink: &mut DoubleXorSink,
                          num_values: usize) -> Result<&'a [u8], NibblePackError> {
    if (encoded.len() < 8) {
        Err(NibblePackError::InputTooShort)
    } else {
        assert!(num_values >= 1);
        let init_value = direct_read_uint_le(encoded, 0);
        sink.reset(init_value);

        unpack(&encoded[8..], sink, num_values - 1)
    }
}

/// Unpacks 8 u64's packed using nibble_pack8 by calling the output.process() method 8 times, once for each encoded
/// value.  Always calls 8 times regardless of what is in the input, unless the input is too short.
/// Returns "remainder" byteslice or unpacking error (say if one ran out of space)
///
/// # Arguments
/// * `inbuf` - NibblePacked compressed byte slice containing "remaining" bytes, starting with bitmask byte
/// * `output` - a Trait which processes each resulting u64
// NOTE: The 'a is a lifetime annotation.  When you use two references in Rust, and return one, Rust needs
//       annotations to help it determine to which input the output lifetime is related to, so Rust knows
//       that the output of the slice will live as long as the reference to the input slice is valid.
#[inline]
fn nibble_unpack8<'a, Output: Sink>(
    inbuf: &'a [u8],
    output: &mut Output,
) -> Result<&'a [u8], NibblePackError> {
    let nonzero_mask = inbuf[0];
    if nonzero_mask == 0 {
        // All 8 words are 0; skip further processing
        output.process8(0);
        Ok(&inbuf[1..])
    } else {
        let num_bits = ((inbuf[1] >> 4) + 1) * 4;
        let trailing_zeros = (inbuf[1] & 0x0f) * 4;
        let total_bytes = 2 + (num_bits as u32 * nonzero_mask.count_ones() + 7) / 8;
        let mask: u64 = if num_bits >= 64 { std::u64::MAX } else { (1u64 << num_bits) - 1u64 };
        let mut buf_index = 2;
        let mut bit_cursor = 0;

        // Read in first word
        let mut in_word = direct_read_uint_le(inbuf, buf_index);
        buf_index += 8;

        output.reserve(8);

        for bit in 0..8 {
            if (nonzero_mask & (1 << bit)) != 0 {
                let remaining = 64 - bit_cursor;

                // Shift and read in LSB (or entire nibbles if they fit)
                let shifted_in = in_word >> bit_cursor;
                let mut out_word = shifted_in & mask;

                // If remaining bits are in next word, read next word -- if there's space
                // We don't want to read the next word though if we're already at the end
                if remaining <= num_bits && buf_index < total_bytes {
                    if ((buf_index as usize) < inbuf.len()) {
                        // Read in MSB bits from next wordå
                        in_word = direct_read_uint_le(inbuf, buf_index);
                        buf_index += 8; // figure out right amount!
                        if remaining < num_bits {
                            let shifted = in_word << remaining;
                            out_word |= shifted & mask;
                        }
                    } else {
                        return Err(NibblePackError::InputTooShort);
                    }
                }

                output.process(out_word << trailing_zeros);

                // Update other indices
                bit_cursor = (bit_cursor + num_bits) % 64;
            } else {
                output.process(0);
            }
        }

        // Return the "remaining slice" - the rest of input buffer after we've parsed our bytes.
        // This allows for easy and clean chaining of nibble_unpack8 calls with no mutable state
        Ok(&inbuf[(total_bytes as usize)..])
    }
}

#[test]
fn nibblepack8_all_zeroes() {
    let mut buf = Vec::with_capacity(512);
    let inputs = [0u64; 8];
    nibble_pack8(&inputs, &mut buf);
    dbg!(is_x86_feature_detected!("avx2"));
    assert_eq!(buf.len(), 1);
    assert_eq!(buf[..], [0u8]);
}

#[rustfmt::skip]
#[test]
fn nibblepack8_all_evennibbles() {
    // All 8 are nonzero, even # nibbles
    let mut buf = Vec::with_capacity(512);
    let inputs = [ 0x0000_00fe_dcba_0000u64, 0x0000_0033_2211_0000u64,
                   0x0000_0044_3322_0000u64, 0x0000_0055_4433_0000u64,
                   0x0000_0066_5544_0000u64, 0x0000_0076_5432_0000u64,
                   0x0000_0087_6543_0000u64, 0x0000_0098_7654_0000u64, ];
    nibble_pack8(&inputs, &mut buf);

    // Expected result:
    let expected_buf = [
        0xffu8, // Every input is nonzero, all bits on
        0x54u8, // six nibbles wide, four zero nibbles trailing
        0xbau8, 0xdcu8, 0xfeu8, 0x11u8, 0x22u8, 0x33u8, 0x22u8, 0x33u8, 0x44u8,
        0x33u8, 0x44u8, 0x55u8, 0x44u8, 0x55u8, 0x66u8, 0x32u8, 0x54u8, 0x76u8,
        0x43u8, 0x65u8, 0x87u8, 0x54u8, 0x76u8, 0x98u8, ];
    assert_eq!(buf.len(), 2 + 3 * 8);
    assert_eq!(buf[..], expected_buf);
}

// Even nibbles with different combos of partial
#[rustfmt::skip]   // We format the arrays specially to help visually see input vs output.  Don't reformat.
#[test]
fn nibblepack8_partial_evennibbles() {
    // All 8 are nonzero, even # nibbles
    let mut buf = Vec::with_capacity(1024);
    let inputs = [
        0u64,
        0x0000_0033_2211_0000u64, 0x0000_0044_3322_0000u64,
        0x0000_0055_4433_0000u64, 0x0000_0066_5544_0000u64,
        0u64,
        0u64,
        0u64,
    ];
    nibble_pack8(&inputs, &mut buf);

    // Expected result:
    let expected_buf = [
        0b0001_1110u8, // only some bits on
        0x54u8,        // six nibbles wide, four zero nibbles trailing
        0x11u8, 0x22u8, 0x33u8, 0x22u8, 0x33u8, 0x44u8,
        0x33u8, 0x44u8, 0x55u8, 0x44u8, 0x55u8, 0x66u8,
    ];
    assert_eq!(buf.len(), 2 + 3 * 4);
    assert_eq!(buf[..], expected_buf);
}

// Odd nibbles with different combos of partial
#[rustfmt::skip]
#[test]
fn nibblepack8_partial_oddnibbles() {
    // All 8 are nonzero, even # nibbles
    let mut buf = Vec::with_capacity(1024);
    let inputs = [
        0u64,
        0x0000_0033_2210_0000u64, 0x0000_0044_3320_0000u64,
        0x0000_0055_4430_0000u64, 0x0000_0066_5540_0000u64,
        0x0000_0076_5430_0000u64, 0u64, 0u64,
    ];
    let res = nibble_pack8(&inputs, &mut buf);

    // Expected result:
    let expected_buf = [
        0b0011_1110u8, // only some bits on
        0x45u8,        // five nibbles wide, five zero nibbles trailing
        0x21u8, 0x32u8, 0x23u8, 0x33u8, 0x44u8, // First two values
        0x43u8, 0x54u8, 0x45u8, 0x55u8, 0x66u8,
        0x43u8, 0x65u8, 0x07u8,
    ];
    assert_eq!(buf.len(), expected_buf.len());
    assert_eq!(buf[..], expected_buf);
}

// Odd nibbles > 8 nibbles
#[rustfmt::skip]
#[test]
fn nibblepack8_partial_oddnibbles_large() {
    // All 8 are nonzero, even # nibbles
    let mut buf = Vec::with_capacity(1024);
    let inputs = [
        0u64,
        0x0005_4433_2211_0000u64, 0x0000_0044_3320_0000u64,
        0x0007_6655_4433_0000u64, 0x0000_0066_5540_0000u64,
        0x0001_9876_5430_0000u64, 0u64, 0u64,
    ];
    nibble_pack8(&inputs, &mut buf);

    // Expected result:
    let expected_buf = [
        0b0011_1110u8, // only some bits on
        0x84u8,        // nine nibbles wide, four zero nibbles trailing
        0x11u8, 0x22u8, 0x33u8, 0x44u8, 0x05u8, 0x32u8, 0x43u8, 0x04u8, 0,
        0x33u8, 0x44u8, 0x55u8, 0x66u8, 0x07u8, 0x54u8, 0x65u8, 0x06u8, 0,
        0x30u8, 0x54u8, 0x76u8, 0x98u8, 0x01u8,
    ];
    assert_eq!(buf[..], expected_buf);
}

#[rustfmt::skip]
#[test]
fn nibblepack8_64bit_numbers() {
    let mut buf = Vec::with_capacity(1024);
    let inputs = [0, 0, -1i32 as u64, -2i32 as u64, 0, -100234i32 as u64, 0, 0];
    nibble_pack8(&inputs, &mut buf);

    let expected_buf = [
        0b0010_1100u8,
        0xf0u8, // all 16 nibbles wide, zero nibbles trailing
        0xffu8, 0xffu8, 0xffu8, 0xffu8, 0xffu8, 0xffu8, 0xffu8, 0xffu8,
        0xfeu8, 0xffu8, 0xffu8, 0xffu8, 0xffu8, 0xffu8, 0xffu8, 0xffu8,
        0x76u8, 0x78u8, 0xfeu8, 0xffu8, 0xffu8, 0xffu8, 0xffu8, 0xffu8,
    ];
    assert_eq!(buf[..], expected_buf);
}

#[test]
fn unpack8_all_zeroes() {
    let compressed_array = [0x00u8];
    let mut sink = LongSink::new();
    let res = nibble_unpack8(&compressed_array, &mut sink);
    assert_eq!(res.unwrap().is_empty(), true);
    assert_eq!(sink.vec.len(), 8);
    assert_eq!(sink.vec[..], [0u64; 8]);
}

#[rustfmt::skip]
#[test]
fn unpack8_input_too_short() {
    let compressed = [
        0b0011_1110u8, // only some bits on
        0x84u8,        // nine nibbles wide, four zero nibbles trailing
        0x11u8, 0x22u8, 0x33u8, 0x44u8, 0x05u8, 0x32u8, 0x43u8, 0x04u8, 0,
        0x33u8, 0x44u8, 0x55u8, 0x66u8, 0x07u8,
    ]; // too short!!
    let mut sink = LongSink::new();
    let res = nibble_unpack8(&compressed, &mut sink);
    assert_eq!(res, Err(NibblePackError::InputTooShort));
}

// Tests the case where nibbles lines up with 64-bit boundaries - edge case
#[test]
fn unpack8_4nibbles_allfull() {
    // 4 nibbles = 2^16, so values < 65536
    let inputs = [65535u64; 8];
    let mut buf = Vec::with_capacity(1024);
    nibble_pack8(&inputs, &mut buf);

    let mut sink = LongSink::new();
    let res = nibble_unpack8(&buf[..], &mut sink);
    assert_eq!(res.unwrap().len(), 0);
    assert_eq!(sink.vec[..], inputs);
}

#[test]
fn unpack8_partial_oddnibbles() {
    let compressed = [
        0b0011_1110u8, // only some bits on
        0x84u8,        // nine nibbles wide, four zero nibbles trailing
        0x11u8, 0x22u8, 0x33u8, 0x44u8, 0x05u8, 0x32u8, 0x43u8, 0x04u8, 0,
        0x33u8, 0x44u8, 0x55u8, 0x66u8, 0x07u8, 0x54u8, 0x65u8, 0x06u8, 0,
        0x30u8, 0x54u8, 0x76u8, 0x98u8, 0x01u8,
        0x00u8, ]; // extra padding... just to test the return value
    let mut sink = LongSink::new();
    let res = nibble_unpack8(&compressed, &mut sink);
    assert_eq!(res.unwrap().len(), 1);
    assert_eq!(sink.vec.len(), 8);

    let orig = [
        0u64,
        0x0005_4433_2211_0000u64, 0x0000_0044_3320_0000u64,
        0x0007_6655_4433_0000u64, 0x0000_0066_5540_0000u64,
        0x0001_9876_5430_0000u64, 0u64, 0u64,
    ];

    assert_eq!(sink.vec[..], orig);
}

#[test]
fn pack_unpack_u64_plain() {
    let inputs = [0u64, 1000, 1001, 1002, 1003, 2005, 2010, 3034, 4045, 5056, 6067, 7078];
    let mut buf = Vec::with_capacity(1024);
    // NOTE: into_iter() of an array returns an Iterator<Item = &u64>, cloned() is needed to convert back to u64
    pack_u64(inputs.into_iter().cloned(), &mut buf);
    println!("Packed {} u64 inputs (plain) into {} bytes", inputs.len(), buf.len());

    let mut sink = LongSink::new();
    let res = unpack(&buf[..], &mut sink, inputs.len());
    assert_eq!(res.unwrap().len(), 0);
    assert_eq!(sink.vec[..inputs.len()], inputs);
}

#[test]
fn pack_unpack_u64_deltas() {
    let inputs = [0u64, 1000, 1001, 1002, 1003, 2005, 2010, 3034, 4045, 5056, 6067, 7078];
    let mut buf = Vec::with_capacity(1024);
    // NOTE: into_iter() of an array returns an Iterator<Item = &u64>, cloned() is needed to convert back to u64
    pack_u64_delta(&inputs[..], &mut buf);
    println!("Packed {} u64 inputs (delta) into {} bytes", inputs.len(), buf.len());

    let mut sink = DeltaSink::new();
    let res = unpack(&buf[..], &mut sink, inputs.len());
    assert_eq!(res.unwrap().len(), 0);
    assert_eq!(sink.sink.vec[..inputs.len()], inputs);
}

#[test]
fn pack_unpack_f64_xor() {
    let inputs = [0f64, 0.5, 2.5, 10., 25., 100.];
    let mut buf = Vec::with_capacity(512);
    pack_f64_xor(inputs.into_iter().cloned(), &mut buf);
    println!("Packed {} f64 inputs (XOR) into {} bytes", inputs.len(), buf.len());

    let mut out = Vec::<f64>::with_capacity(64);
    let mut sink = DoubleXorSink::new(out);
    let res = unpack_f64_xor(&buf[..], &mut sink, inputs.len());
    assert_eq!(res.unwrap().len(), 0);
    assert_eq!(sink.vec[..inputs.len()], inputs);
}

#[test]
fn delta_diffpack_sink_test() {
    let inputs = [ [0u64, 1000, 1001, 1002, 1003, 2005, 2010, 3034, 4045, 5056, 6067, 7078],
                   [3u64, 1004, 1006, 1008, 1009, 2012, 2020, 3056, 4070, 5090, 6101, 7150],
                   // [3u64, 1004, 1006, 1008, 1009, 2010, 2020, 3056, 4070, 5090, 6101, 7150],
                   [7u64, 1010, 1016, 1018, 1019, 2022, 2030, 3078, 4101, 5122, 6134, 7195] ];
    let diffs = inputs.windows(2).map(|pair| {
        pair[1].iter().zip(pair[0].iter()).map(|(nb, na)| nb - na ).collect::<Vec<_>>()
    }).collect::<Vec<_>>();

    // Compress each individual input into its own buffer
    let compressed_inputs: Vec<Vec<u8>> = inputs.iter().map(|input| {
        let mut buf = Vec::with_capacity(128);
        pack_u64_delta(&input[..], &mut buf);
        buf
    }).collect();

    let out_buf = Vec::<u8>::new();
    let mut sink = DeltaDiffPackSink::new(inputs[0].len(), out_buf);

    // Verify delta on first one (empty diffs) yields back the original
    let res = unpack(&compressed_inputs[0][..], &mut sink, inputs[0].len());
    assert_eq!(res.unwrap().len(), 0);
    sink.finish();

    let mut dsink = DeltaSink::new();
    let res = unpack(&sink.out_vec[..], &mut dsink, inputs[0].len());
    assert_eq!(dsink.sink.vec[..inputs[0].len()], inputs[0]);

    // Second and subsequent inputs shouyld correspond to diffs
    for i in 1..3 {
        sink.out_vec.truncate(0);   // need to reset output
        let res = unpack(&compressed_inputs[i][..], &mut sink, inputs[0].len());
        assert_eq!(res.unwrap().len(), 0);
        assert_eq!(sink.value_dropped, false);  // should not have dropped?
        sink.finish();
        // dbg!(&sink.out_vec);

        let mut dsink = DeltaSink::new();
        let res = unpack(&sink.out_vec[..], &mut dsink, inputs[0].len());
        assert_eq!(dsink.sink.vec[..inputs[0].len()], diffs[i - 1][..]);
    }
}

// NOTE: cfg(test) is needed so that proptest can just be a "dev-dependency" and not linked for final library
// NOTE2: somehow cargo is happier when we put props tests in its own module
#[cfg(test)]
mod props {
    extern crate proptest;

    use self::proptest::prelude::*;
    use super::*;

    // Generators (Arb's) for numbers of given # bits with fractional chance of being zero.
    // Also input arrays of 8 with the given properties above.
    prop_compose! {
        /// zero_chance: 0..1.0 chance of obtaining a zero
        fn arb_maybezero_nbits_u64
            (nbits: usize, zero_chance: f32)
            (is_zero in prop::bool::weighted(zero_chance as f64),
             n in 0u64..(1 << nbits))
            -> u64
        {
            if is_zero { 0 } else { n }
        }
    }

    // Try different # bits and # nonzero elements
    prop_compose! {
        fn arb_8longs_nbits()
                           (nbits in 4usize..64, chance in 0.2f32..0.8)
                           (input in prop::array::uniform8(arb_maybezero_nbits_u64(nbits, chance))) -> [u64; 8] {
                               input
        }
    }

    // Generate variable length increasing/deltas u64's
    prop_compose! {
        fn arb_varlen_deltas()
                            (nbits in 4usize..48, chance in 0.2f32..0.8)
                            (mut v in proptest::collection::vec(arb_maybezero_nbits_u64(nbits, chance), 2..64)) -> Vec<u64> {
            for i in 1..v.len() {
                // make numbers increasing
                v[i] = v[i - 1] + v[i];
            }
            v
        }
    }

    proptest! {
        #[test]
        fn prop_pack_unpack_identity(input in arb_8longs_nbits()) {
            let mut buf = Vec::with_capacity(256);
            nibble_pack8(&input, &mut buf);

            let mut sink = LongSink::new();
            let res = nibble_unpack8(&buf[..], &mut sink);
            assert_eq!(sink.vec[..], input);
        }

        #[test]
        fn prop_delta_u64s_packing(input in arb_varlen_deltas()) {
            let mut buf = Vec::with_capacity(1024);
            pack_u64_delta(&input[..], &mut buf);
            let mut sink = DeltaSink::new();
            let res = unpack(&buf[..], &mut sink, input.len());
            assert_eq!(sink.sink.vec[..input.len()], input[..]);
        }
    }
}
