use std::fs;
use std::path::PathBuf;

use super::settings::NovelSettings;

pub const INSPECT_LOG_NAME: &str = "調査ログ.txt";
const LINE_LENGTH_THRESHOLD: usize = 400;
const BRACKETS_RETURN_COUNT_THRESHOLD: usize = 7;
const END_TOUTEN_COUNT_THRESHOLD: usize = 50;
const AUTO_INDENT_THRESHOLD_RATIO: f64 = 0.5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MessageLevel {
    Error,
    Warning,
    Info,
}

impl MessageLevel {
    fn tag(self) -> &'static str {
        match self {
            Self::Error => "エラー",
            Self::Warning => "警告",
            Self::Info => "INFO",
        }
    }
}

#[derive(Debug, Clone)]
struct Message {
    level: MessageLevel,
    body: String,
}

pub struct Inspector {
    archive_path: PathBuf,
    messages: Vec<Message>,
    subtitle: String,
}

impl Inspector {
    pub fn new(settings: &NovelSettings) -> Self {
        Self {
            archive_path: settings.archive_path.clone(),
            messages: Vec::new(),
            subtitle: String::new(),
        }
    }

    pub fn reset(&mut self) {
        self.messages.clear();
        self.subtitle.clear();
    }

    pub fn set_subtitle(&mut self, subtitle: impl Into<String>) {
        self.subtitle = subtitle.into();
    }

    pub fn save(&self) -> std::io::Result<()> {
        let mut output = format!("※調査日時：{}\n", chrono::Local::now());
        let rendered = self.render_filtered(|_| true);
        if !rendered.is_empty() {
            output.push('\n');
            output.push_str(&rendered);
        }
        fs::create_dir_all(&self.archive_path)?;
        fs::write(self.archive_path.join(INSPECT_LOG_NAME), output)
    }

    pub fn summary_text(&self) -> Option<String> {
        if self.messages.is_empty() {
            return None;
        }

        Some(format!(
            "小説状態の調査結果を {} に出力しました（{}）",
            INSPECT_LOG_NAME,
            [
                MessageLevel::Error,
                MessageLevel::Warning,
                MessageLevel::Info,
            ]
            .iter()
            .map(|level| format!("{}：{}件", level.tag(), self.count(*level)))
            .collect::<Vec<_>>()
            .join("、")
        ))
    }

    pub fn display_text(&self) -> Option<String> {
        let mut sections = Vec::new();

        let errors_and_warnings = self
            .render_filtered(|level| matches!(level, MessageLevel::Error | MessageLevel::Warning));
        if !errors_and_warnings.is_empty() {
            sections.push(format!("※警告・エラー\n{}", errors_and_warnings));
        }

        let info = self.render_filtered(|level| level == MessageLevel::Info);
        if !info.is_empty() {
            sections.push(format!("※情報\n{}", info));
        }

        if sections.is_empty() {
            None
        } else {
            Some(sections.join("\n"))
        }
    }

    pub fn inspect_end_touten_conditions(&mut self, data: &str, auto_join_line_enabled: bool) {
        if auto_join_line_enabled {
            return;
        }

        let count = data.matches("、\n　").count();
        if count == 0 {
            return;
        }

        let mut message = format!("{}個の行末読点を発見しました。", count);
        if count >= END_TOUTEN_COUNT_THRESHOLD {
            message.push_str(
                "作者による手動改行により改行が多くなっています。setting.ini の enable_auto_join_line を true にすることをお薦めします。",
            );
        }
        self.info(message);
    }

    pub fn countup_return_in_brackets(&mut self, data: &str, auto_join_in_brackets_enabled: bool) {
        if auto_join_in_brackets_enabled {
            return;
        }

        let mut max = 0;
        let mut brackets_num = 0;
        let mut brackets_num_over_threshold = 0;
        let mut total = 0;

        for (open, close) in [('「', '」'), ('『', '』')] {
            for enclosed in self.extract_balanced_contents(data, open, close) {
                let count = enclosed.matches('\n').count();
                brackets_num += 1;
                total += count;
                if count >= BRACKETS_RETURN_COUNT_THRESHOLD {
                    brackets_num_over_threshold += 1;
                }
                if count > max {
                    max = count;
                }
            }
        }

        self.info(format!(
            "カギ括弧内の改行状況:\n検出したカギ括弧数: {}、そのうち{}個以上改行を含む数: {}\n1つのカギ括弧内で最大の改行数: {}、全カギ括弧内での改行合計: {}",
            brackets_num,
            BRACKETS_RETURN_COUNT_THRESHOLD,
            brackets_num_over_threshold,
            max,
            total
        ));
    }

    pub fn should_auto_indent(data: &str) -> bool {
        let mut target_line_count = 0usize;
        let mut dont_indent_line_count = 0usize;

        for line in data.split('\n') {
            let Some(head) = line.chars().next() else {
                continue;
            };
            if is_ignore_indent_char(head) {
                continue;
            }
            target_line_count += 1;
            if head != ' ' && head != '\u{3000}' {
                dont_indent_line_count += 1;
            }
        }

        if target_line_count == 0 {
            return false;
        }

        (dont_indent_line_count as f64 / target_line_count as f64) > AUTO_INDENT_THRESHOLD_RATIO
    }

    pub fn info(&mut self, body: impl Into<String>) {
        self.messages.push(Message {
            level: MessageLevel::Info,
            body: body.into(),
        });
    }

    pub fn warning(&mut self, body: impl Into<String>) {
        self.messages.push(Message {
            level: MessageLevel::Warning,
            body: body.into(),
        });
    }

    pub fn error(&mut self, body: impl Into<String>) {
        self.messages.push(Message {
            level: MessageLevel::Error,
            body: body.into(),
        });
    }

    pub fn validate_joined_inner_brackets(
        &mut self,
        raw_strings: &str,
        joined_strings: &str,
    ) -> bool {
        if raw_strings.matches('\n').count() >= BRACKETS_RETURN_COUNT_THRESHOLD {
            self.warning(format!(
                "改行が規定の回数を超えて検出されました。作者による意図的な改行とみなし、連結を中止しました。\n{}",
                self.omit_message(raw_strings)
            ));
            return true;
        }

        if joined_strings.chars().count() >= LINE_LENGTH_THRESHOLD {
            self.warning(format!(
                "連結結果が長過ぎます。連結を中止しました。特殊な用途(手紙形式)等でかぎ括弧が使われている可能性があります。\n{}",
                self.omit_message(raw_strings)
            ));
            return true;
        }

        false
    }

    pub fn inspect_invalid_openclose_brackets(
        &mut self,
        data: &str,
        open: char,
        close: char,
        replacements: &[String],
    ) {
        for bracket in [open, close] {
            let mut buffer = data.to_string();
            while let Some((index, _)) = buffer.char_indices().find(|(_, ch)| *ch == bracket) {
                let before = rebuild_brackets(&buffer[..index], replacements);
                let after = rebuild_brackets(&buffer[index + bracket.len_utf8()..], replacements);
                let snippet = format!("{}{}{}", tail_chars(&before, 15), bracket, after);
                self.error(format!(
                    "かぎ括弧({})が正しく閉じていません。\n{}",
                    bracket,
                    self.omit_message(&snippet)
                ));
                buffer.truncate(index);
            }
        }
    }

    fn count(&self, level: MessageLevel) -> usize {
        self.messages
            .iter()
            .filter(|msg| msg.level == level)
            .count()
    }

    fn render_filtered<F>(&self, filter: F) -> String
    where
        F: Fn(MessageLevel) -> bool,
    {
        self.messages
            .iter()
            .filter(|msg| filter(msg.level))
            .map(|msg| format!("[{}] {}", msg.level.tag(), msg.body))
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    fn extract_balanced_contents<'a>(
        &self,
        data: &'a str,
        open: char,
        close: char,
    ) -> Vec<&'a str> {
        let mut stack = Vec::new();
        let mut results = Vec::new();

        for (idx, ch) in data.char_indices() {
            if ch == open {
                stack.push(idx + ch.len_utf8());
            } else if ch == close {
                if let Some(start) = stack.pop() {
                    results.push(&data[start..idx]);
                }
            }
        }

        results
    }

    fn omit_message(&self, strings: &str) -> String {
        let navigation = if self.subtitle.is_empty() {
            String::new()
        } else {
            format!("in {}", self.subtitle)
        };
        format!(
            "≫≫≫ 該当箇所 {}\n...{}...",
            navigation,
            strings
                .chars()
                .take(36)
                .collect::<String>()
                .replace('\n', "\\n")
        )
    }
}

fn is_ignore_indent_char(ch: char) -> bool {
    matches!(
        ch,
        '(' | '（'
            | '「'
            | '『'
            | '〈'
            | '《'
            | '≪'
            | '【'
            | '〔'
            | '―'
            | '・'
            | '※'
            | '［'
            | '〝'
    )
}

fn rebuild_brackets(data: &str, replacements: &[String]) -> String {
    let re = regex::Regex::new(r"［＃かぎ括弧＝(\d+)］").unwrap();
    re.replace_all(data, |caps: &regex::Captures| {
        let index = caps[1].parse::<usize>().unwrap_or(usize::MAX);
        replacements
            .get(index)
            .cloned()
            .unwrap_or_else(|| caps[0].to_string())
    })
    .to_string()
}

fn tail_chars(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        text.to_string()
    } else {
        text.chars().skip(count - max_chars).collect()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{END_TOUTEN_COUNT_THRESHOLD, INSPECT_LOG_NAME, Inspector};
    use crate::converter::settings::NovelSettings;

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(1);

    fn test_settings() -> NovelSettings {
        let mut settings = NovelSettings::default();
        settings.archive_path = std::env::temp_dir().join(format!(
            "narou-rs-inspector-test-{}-{}",
            TEST_COUNTER.fetch_add(1, Ordering::Relaxed),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&settings.archive_path).unwrap();
        settings
    }

    #[test]
    fn should_auto_indent_uses_ruby_threshold() {
        assert!(!Inspector::should_auto_indent(
            "　字下げ済み\n　字下げ済み\n未字下げ"
        ));
        assert!(Inspector::should_auto_indent(
            "字下げなし\n字下げなし\n　字下げ済み"
        ));
    }

    #[test]
    fn inspect_end_touten_conditions_reports_recommendation_when_many() {
        let settings = test_settings();
        let mut inspector = Inspector::new(&settings);
        let text = "A、\n　".repeat(END_TOUTEN_COUNT_THRESHOLD);

        inspector.inspect_end_touten_conditions(&text, false);

        let summary = inspector.summary_text().unwrap();
        assert!(summary.contains("INFO：1件"));

        let display = inspector.display_text().unwrap();
        assert!(display.contains("※情報"));
        assert!(display.contains("50個の行末読点を発見しました。"));
        assert!(display.contains("enable_auto_join_line を true にすることをお薦めします。"));

        let _ = std::fs::remove_dir_all(settings.archive_path);
    }

    #[test]
    fn countup_return_in_brackets_reports_counts() {
        let settings = test_settings();
        let mut inspector = Inspector::new(&settings);
        let text = "「一行目\n二行目\n三行目」\n『a\nb』";

        inspector.countup_return_in_brackets(text, false);

        let display = inspector.display_text().unwrap();
        assert!(display.contains("検出したカギ括弧数: 2"));
        assert!(display.contains("全カギ括弧内での改行合計: 3"));

        let _ = std::fs::remove_dir_all(settings.archive_path);
    }

    #[test]
    fn save_writes_inspect_log() {
        let settings = test_settings();
        let mut inspector = Inspector::new(&settings);
        inspector.inspect_end_touten_conditions("一、\n　", false);
        inspector.save().unwrap();

        let path = settings.archive_path.join(INSPECT_LOG_NAME);
        let content = std::fs::read_to_string(path).unwrap();
        assert!(content.contains("※調査日時："));
        assert!(content.contains("[INFO] 1個の行末読点を発見しました。"));

        let _ = std::fs::remove_dir_all(settings.archive_path);
    }
}
