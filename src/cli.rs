use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "narou", about = "narou.rs - A Rust port of narou.rb")]
pub struct Cli {
    #[arg(long, global = true)]
    pub user_agent: Option<String>,
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(clap::Subcommand, Debug)]
pub enum Commands {
    Init {
        #[arg(short = 'p', long = "path")]
        aozora_path: Option<String>,
        #[arg(short = 'l', long = "line-height")]
        line_height: Option<f64>,
    },
    Web {
        #[arg(short, long, default_value_t = 3000)]
        port: u16,
        #[arg(short, long, default_value_t = false)]
        no_browser: bool,
    },
    Download {
        targets: Vec<String>,
    },
    Update {
        ids: Option<Vec<String>>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        no_convert: bool,
        #[arg(long)]
        sort_by: Option<String>,
    },
    Convert {
        targets: Vec<String>,
    },
    List {
        #[arg(short, long)]
        tag: Option<String>,
        #[arg(long)]
        frozen: bool,
    },
    Tag {
        #[arg(short, long)]
        add: Option<String>,
        #[arg(short, long)]
        remove: Option<String>,
        targets: Vec<String>,
    },
    Freeze {
        targets: Vec<String>,
        #[arg(long)]
        off: bool,
    },
    Remove {
        targets: Vec<String>,
    },
    Setting {
        args: Vec<String>,
        #[arg(short, long)]
        list: bool,
        #[arg(short, long)]
        all: bool,
        #[arg(long)]
        burn: bool,
    },
}
