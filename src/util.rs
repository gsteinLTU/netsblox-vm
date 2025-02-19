use alloc::string::String;

pub struct LosslessJoin {
    content: String,
}
impl LosslessJoin {
    pub fn new() -> Self {
        Self { content: String::new() }
    }
    pub fn push(&mut self, val: &str) {
        assert!(val.as_bytes().iter().all(|&x| x != 0));

        self.content.push('\0');
        self.content.push_str(val);
    }
    pub fn finish(self) -> String {
        self.content
    }
}

pub fn lossless_split(src: &str) -> impl Iterator<Item = &str> {
    assert!(src.chars().next().unwrap_or('\0') == '\0');
    src.split('\0').skip(1)
}

#[test]
fn test_lossless_split() {
    fn assert_round_trip(input: &[&str], output: &str) {
        let mut res = LosslessJoin::new();
        for x in input {
            res.push(x);
        }
        let res = res.finish();
        assert_eq!(res, output);
        let back = lossless_split(&res).collect::<alloc::vec::Vec<_>>();
        assert_eq!(back, input);
    }

    assert_round_trip(&[], "");
    assert_round_trip(&[""], "\0");
    assert_round_trip(&["", ""], "\0\0");
    assert_round_trip(&["test"], "\0test");
    assert_round_trip(&["test", ""], "\0test\0");
    assert_round_trip(&["test", "", "merp"], "\0test\0\0merp");
    assert_round_trip(&["test", "", "merp", ""], "\0test\0\0merp\0");
    assert_round_trip(&["", "test", "", "merp", ""], "\0\0test\0\0merp\0");
}