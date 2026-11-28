#[derive(Default)]
pub struct SseDecoder {
    pending: Vec<u8>,
    data: String,
    has_data: bool,
}

impl SseDecoder {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.pending.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(end) = self.pending.iter().position(|byte| *byte == b'\n') {
            let mut line = self.pending.drain(..=end).collect::<Vec<u8>>();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.line(&String::from_utf8_lossy(&line), &mut events);
        }
        events
    }

    pub fn finish(&mut self) -> Vec<String> {
        let mut events = Vec::new();
        if !self.pending.is_empty() {
            let line = String::from_utf8_lossy(&std::mem::take(&mut self.pending)).into_owned();
            self.line(line.trim_end_matches('\r'), &mut events);
        }
        self.line("", &mut events);
        events
    }

    fn line(&mut self, line: &str, events: &mut Vec<String>) {
        if line.is_empty() {
            if self.has_data {
                events.push(std::mem::take(&mut self.data));
                self.has_data = false;
            }
            return;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            if self.has_data {
                self.data.push('\n');
            }
            self.data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
            self.has_data = true;
        }
    }
}
