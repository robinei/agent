pub enum ThinkingSegment {
    Thinking(String),
    Text(String),
}

pub fn split_thinking_chunks(text: &str, in_thinking: &mut bool) -> Vec<ThinkingSegment> {
    let mut result = Vec::new();
    let mut rest = text;
    loop {
        if *in_thinking {
            match rest.find("</think>") {
                Some(pos) => {
                    if pos > 0 {
                        result.push(ThinkingSegment::Thinking(rest[..pos].to_string()));
                    }
                    *in_thinking = false;
                    rest = &rest[pos + "</think>".len()..];
                }
                None => {
                    if !rest.is_empty() {
                        result.push(ThinkingSegment::Thinking(rest.to_string()));
                    }
                    break;
                }
            }
        } else {
            match rest.find("<think>") {
                Some(pos) => {
                    if pos > 0 {
                        result.push(ThinkingSegment::Text(rest[..pos].to_string()));
                    }
                    *in_thinking = true;
                    rest = &rest[pos + "<think>".len()..];
                }
                None => {
                    if !rest.is_empty() {
                        result.push(ThinkingSegment::Text(rest.to_string()));
                    }
                    break;
                }
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_no_tags() {
        let mut in_thinking = false;
        let result = split_thinking_chunks("hello world", &mut in_thinking);
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], ThinkingSegment::Text(t) if t == "hello world"));
        assert!(!in_thinking);
    }

    #[test]
    fn test_split_full_block() {
        let mut in_thinking = false;
        let result = split_thinking_chunks("<think>reason</think>answer", &mut in_thinking);
        assert_eq!(result.len(), 2);
        assert!(matches!(&result[0], ThinkingSegment::Thinking(t) if t == "reason"));
        assert!(matches!(&result[1], ThinkingSegment::Text(t) if t == "answer"));
        assert!(!in_thinking);
    }

    #[test]
    fn test_split_open_only() {
        let mut in_thinking = false;
        let result = split_thinking_chunks("<think>partial", &mut in_thinking);
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], ThinkingSegment::Thinking(t) if t == "partial"));
        assert!(in_thinking);
    }

    #[test]
    fn test_split_close_only() {
        let mut in_thinking = true;
        let result = split_thinking_chunks("end</think>rest", &mut in_thinking);
        assert_eq!(result.len(), 2);
        assert!(matches!(&result[0], ThinkingSegment::Thinking(t) if t == "end"));
        assert!(matches!(&result[1], ThinkingSegment::Text(t) if t == "rest"));
        assert!(!in_thinking);
    }

    #[test]
    fn test_split_empty_think_block() {
        let mut in_thinking = false;
        let result = split_thinking_chunks("<think></think>after", &mut in_thinking);
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], ThinkingSegment::Text(t) if t == "after"));
        assert!(!in_thinking);
    }
}