pub fn checking(url: &str) -> String { format!("作者を確認します: {url}") }
pub fn queued(url: &str, count: usize) -> String { format!("作者 {url}: 新規作品 {count} 件の取得を登録しました") }
pub fn untracked(url: &str) -> String { format!("作者 {url} は追跡対象から外れたため確認をスキップしました") }
