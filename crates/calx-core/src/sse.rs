//! A minimal server-sent events parser: feed it chunks of bytes as they arrive
//! and get back the `data` payload of each complete event.

#[derive(Default)]
pub struct SseParser {
    buf: Vec<u8>,
}

impl SseParser {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        // Normalizing CRLF here is safe even when a chunk ends between \r and \n,
        // because a lone \r is dropped either way.
        self.buf
            .extend(chunk.iter().copied().filter(|&b| b != b'\r'));
        let mut events = Vec::new();
        while let Some(end) = self.buf.windows(2).position(|w| w == b"\n\n") {
            let block: Vec<u8> = self.buf.drain(..end + 2).collect();
            let block = String::from_utf8_lossy(&block);
            let data: Vec<&str> = block
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(|d| d.strip_prefix(' ').unwrap_or(d))
                .collect();
            if !data.is_empty() {
                events.push(data.join("\n"));
            }
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reassembles_events_split_across_chunks() {
        let stream =
            "event: a\r\ndata: {\"x\":\"é\"}\r\n\r\n: keep-alive\n\ndata: line1\ndata: line2\n\n";
        let bytes = stream.as_bytes();
        // Split inside the two-byte UTF-8 "é" and between \r and \n.
        let cut1 = stream.find('é').unwrap() + 1;
        let cut2 = stream.find("\r\n\r\n").unwrap() + 1;
        let mut parser = SseParser::default();
        let mut events = parser.push(&bytes[..cut1]);
        events.extend(parser.push(&bytes[cut1..cut2]));
        events.extend(parser.push(&bytes[cut2..]));
        assert_eq!(events, vec!["{\"x\":\"é\"}", "line1\nline2"]);
    }
}
