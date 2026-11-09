//! A synchronous codec may retain one frame, never an animation. Its output is
//! admitted in fixed chunks, without large Vec growth/reallocation peaks.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use super::CHUNK;

pub(super) type FrameChunks = VecDeque<Vec<u8>>;
struct Buffer { chunks: FrameChunks, allocated: usize, limit: usize }
#[derive(Clone)]
pub(super) struct FrameSink(Arc<Mutex<Buffer>>);
impl FrameSink {
    pub fn new() -> Self { Self(Arc::new(Mutex::new(Buffer { chunks: VecDeque::new(), allocated: 0, limit: usize::MAX }))) }
    pub fn set_limit(&self, limit: usize) -> io::Result<()> {
        let mut buffer = self.0.lock().map_err(|_| io::Error::other("frame output lock poisoned"))?;
        if buffer.allocated > limit { return Err(io::Error::other("frame output exceeds Worker memory budget")); }
        buffer.limit = limit; Ok(())
    }
    pub fn take(&self) -> FrameChunks {
        let mut buffer = self.0.lock().expect("frame output lock poisoned");
        buffer.allocated = 0;
        std::mem::take(&mut buffer.chunks)
    }
}
impl Write for FrameSink {
    fn write(&mut self, mut bytes: &[u8]) -> io::Result<usize> {
        let length = bytes.len();
        let mut buffer = self.0.lock().map_err(|_| io::Error::other("frame output lock poisoned"))?;
        let spare = buffer.chunks.back().map_or(0, |chunk| CHUNK - chunk.len());
        let additional = bytes.len().saturating_sub(spare).div_ceil(CHUNK).checked_mul(CHUNK)
            .and_then(|bytes| buffer.allocated.checked_add(bytes)).ok_or_else(|| io::Error::other("frame output size overflow"))?;
        if additional > buffer.limit { return Err(io::Error::other("frame output exceeds Worker memory budget")); }
        while !bytes.is_empty() {
            if buffer.chunks.back().is_none_or(|chunk| chunk.len() == CHUNK) {
                let required = buffer.allocated.checked_add(CHUNK).ok_or_else(|| io::Error::other("frame output size overflow"))?;
                if let Err(error) = buffer.chunks.try_reserve(1) {
                    return if bytes.len() == length { Err(io::Error::other(error)) } else { Ok(length - bytes.len()) };
                }
                let mut chunk = Vec::new();
                if let Err(error) = chunk.try_reserve_exact(CHUNK) {
                    return if bytes.len() == length { Err(io::Error::other(error)) } else { Ok(length - bytes.len()) };
                }
                buffer.allocated = required; buffer.chunks.push_back(chunk);
            }
            let chunk = buffer.chunks.back_mut().unwrap();
            let count = bytes.len().min(CHUNK - chunk.len());
            chunk.extend_from_slice(&bytes[..count]); bytes = &bytes[count..];
        }
        Ok(length)
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

#[cfg(test)]
pub(super) fn test_chunks(bytes: Vec<u8>) -> FrameChunks { bytes.chunks(CHUNK).map(<[u8]>::to_vec).collect() }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_output_is_chunked_and_admitted_before_growth() {
        let mut sink = FrameSink::new();
        sink.set_limit(CHUNK * 2).unwrap();
        sink.write_all(&vec![7; CHUNK + 3]).unwrap();
        let chunks = sink.take();
        assert_eq!(chunks.iter().map(Vec::len).collect::<Vec<_>>(), [CHUNK, 3]);
        sink.set_limit(CHUNK).unwrap();
        assert!(sink.write_all(&vec![0; CHUNK + 1]).is_err());
        assert!(sink.take().is_empty());
    }
}
