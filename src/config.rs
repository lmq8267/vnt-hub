use clap::Parser;

#[derive(Clone, Debug, Parser)]
#[command(name = "vnt-hub", version)]
pub struct Args {
    /// Web控制台主页 HTTP/HTTPS 复用监听地址
    #[arg(short = 'a', long = "listen", default_value = "0.0.0.0:29876")]
    pub listen: String,
    /// 客户端接入 HTTP/HTTPS/WS/WSS 复用监听地址
    #[arg(short = 'c', long = "console-listen", default_value = "0.0.0.0:29878")]
    pub console_listen: String,
    /// SQLite 数据库路径
    #[arg(short = 'd', long = "db", default_value = "./vnt-hub.db")]
    pub db: String,
    /// 禁止新用户注册
    #[arg(short = 'R', long = "disable-register")]
    pub disable_register: bool,
    /// log路径，为/dev/null时不输出log，为console时输出到控制台
    #[arg(short = 'l', long = "log-path", default_value = "console")]
    pub log_path: String,
}
