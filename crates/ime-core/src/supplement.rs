//! `supplement`: 现代高频口语、金融对公/私行、育儿日常与社会高频构词补充词表
//!
//! 背景：基础开源词库（`rime-ice`）主要源自书面语料，在部分高频日常口语（如「咋呀」「接娃」「遛娃」）、
//! 金融实务简称（如「公户」「私户」「私行」「私行客户」）及现代高频复合词（如「富人区」「公家饭」「纯躺平」）
//! 上存在词条空白。本模块在进程内构建零拷贝紧凑静态 `FlatTrie` 索引（毫秒内初始化一次），
//! 在不改动远端发布词库镜像（保持 SHA-256 完全兼容）的前提下，补齐词组检索、简拼与末尾半音节联想。

use crate::lattice::{EdgeKind, LatticeEdge};
use crate::pinyin::{is_unapostrophed_single_syllable, SyllableSpan};
use ime_lexicon::{DomainTag, FlatTrie};
use std::sync::LazyLock;

#[derive(Debug, Clone, Copy)]
pub struct SupplementEntry {
    pub syllable_key: &'static str,
    pub text: &'static str,
    pub cost: f32,
    pub domain: DomainTag,
}

/// 现代高频口语、金融、育儿、职场及常见熟语补充表（`AIMELX03` 代价标尺：`10.0 ~ 12.2`）
pub static SUPPLEMENT_ENTRIES: &[SupplementEntry] = &[
    // 1. 金融财税 / 银行对公与私行实务
    SupplementEntry { syllable_key: "gong'hu", text: "公户", cost: 10.75, domain: DomainTag::General },
    SupplementEntry { syllable_key: "si'hu", text: "私户", cost: 10.95, domain: DomainTag::General },
    SupplementEntry { syllable_key: "dui'gong'hu", text: "对公户", cost: 11.05, domain: DomainTag::General },
    SupplementEntry { syllable_key: "dui'si'hu", text: "对私户", cost: 11.35, domain: DomainTag::General },
    SupplementEntry { syllable_key: "dui'gong'zhang'hu", text: "对公账户", cost: 10.65, domain: DomainTag::General },
    SupplementEntry { syllable_key: "ji'ben'hu", text: "基本户", cost: 11.25, domain: DomainTag::General },
    SupplementEntry { syllable_key: "yi'ban'hu", text: "一般户", cost: 11.45, domain: DomainTag::General },
    SupplementEntry { syllable_key: "kai'gong'hu", text: "开公户", cost: 11.35, domain: DomainTag::General },
    SupplementEntry { syllable_key: "zou'gong'hu", text: "走公户", cost: 11.35, domain: DomainTag::General },
    SupplementEntry { syllable_key: "zhuan'gong'hu", text: "转公户", cost: 11.45, domain: DomainTag::General },
    SupplementEntry { syllable_key: "si'hang", text: "私行", cost: 11.05, domain: DomainTag::General },
    SupplementEntry { syllable_key: "si'hang'ke'hu", text: "私行客户", cost: 10.65, domain: DomainTag::General },
    SupplementEntry { syllable_key: "si'ren'yin'hang", text: "私人银行", cost: 10.65, domain: DomainTag::General },
    SupplementEntry { syllable_key: "cheng'shang'hang", text: "城商行", cost: 11.15, domain: DomainTag::General },
    SupplementEntry { syllable_key: "nong'shang'hang", text: "农商行", cost: 11.15, domain: DomainTag::General },
    SupplementEntry { syllable_key: "guo'you'hang", text: "国有行", cost: 11.25, domain: DomainTag::General },
    SupplementEntry { syllable_key: "gu'fen'hang", text: "股份行", cost: 11.25, domain: DomainTag::General },
    SupplementEntry { syllable_key: "san'qi'fen'zhang", text: "三七分账", cost: 10.95, domain: DomainTag::General },
    SupplementEntry { syllable_key: "san'qi'fen'cheng", text: "三七分成", cost: 10.85, domain: DomainTag::General },
    SupplementEntry { syllable_key: "wu'wu'fen'zhang", text: "五五分账", cost: 11.15, domain: DomainTag::General },
    SupplementEntry { syllable_key: "si'liu'fen'zhang", text: "四六分账", cost: 11.25, domain: DomainTag::General },
    SupplementEntry { syllable_key: "er'ba'fen'zhang", text: "二八分账", cost: 11.25, domain: DomainTag::General },
    SupplementEntry { syllable_key: "ru'shu'feng'huan", text: "如数奉还", cost: 10.75, domain: DomainTag::General },

    // 2. 家庭育儿与生活日常（`-娃` 系列等）
    SupplementEntry { syllable_key: "jie'wa", text: "接娃", cost: 10.85, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "song'wa", text: "送娃", cost: 11.05, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "dai'wa", text: "带娃", cost: 10.65, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "liu'wa", text: "遛娃", cost: 10.85, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "shai'wa", text: "晒娃", cost: 10.95, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "ji'wa", text: "鸡娃", cost: 11.05, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "hong'wa", text: "哄娃", cost: 11.05, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "sheng'wa", text: "生娃", cost: 10.95, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "yang'wa", text: "养娃", cost: 10.95, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "kan'wa", text: "看娃", cost: 11.15, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "bao'wa", text: "抱娃", cost: 11.25, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "pei'wa", text: "陪娃", cost: 11.15, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "meng'wa", text: "萌娃", cost: 11.05, domain: DomainTag::Chat },

    // 3. 高频日常口语与语气助词短语
    SupplementEntry { syllable_key: "za'ya", text: "咋呀", cost: 10.85, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "za'la", text: "咋啦", cost: 10.65, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "za'le", text: "咋了", cost: 10.55, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "za'zheng", text: "咋整", cost: 10.85, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "za'ban", text: "咋办", cost: 10.75, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "za'shuo", text: "咋说", cost: 10.95, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "za'xiang", text: "咋想", cost: 11.05, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "za'nong", text: "咋弄", cost: 11.05, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "za'gao", text: "咋搞", cost: 11.05, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "hao'da", text: "好哒", cost: 10.95, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "shi'da", text: "是哒", cost: 11.25, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "hao'ya", text: "好呀", cost: 10.65, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "dui'ya", text: "对呀", cost: 10.75, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "xing'ya", text: "行呀", cost: 10.85, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "shi'ya", text: "是呀", cost: 10.85, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "lai'ya", text: "来呀", cost: 10.95, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "qu'ya", text: "去呀", cost: 10.95, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "zou'ya", text: "走呀", cost: 10.95, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "gan'ma'ya", text: "干嘛呀", cost: 10.85, domain: DomainTag::Chat },

    // 4. 区域场所、职场生态与现代社会常用词
    SupplementEntry { syllable_key: "fu'ren'qu", text: "富人区", cost: 10.65, domain: DomainTag::General },
    SupplementEntry { syllable_key: "qiong'ren'qu", text: "穷人区", cost: 11.25, domain: DomainTag::General },
    SupplementEntry { syllable_key: "hua'ren'qu", text: "华人区", cost: 10.95, domain: DomainTag::General },
    SupplementEntry { syllable_key: "wu'ren'qu", text: "无人区", cost: 10.65, domain: DomainTag::General },
    SupplementEntry { syllable_key: "bie'shu'qu", text: "别墅区", cost: 10.85, domain: DomainTag::General },
    SupplementEntry { syllable_key: "ping'lun'qu", text: "评论区", cost: 10.45, domain: DomainTag::General },
    SupplementEntry { syllable_key: "huan'chong'qu", text: "缓冲区", cost: 10.85, domain: DomainTag::General },
    SupplementEntry { syllable_key: "shu'shi'qu", text: "舒适区", cost: 10.85, domain: DomainTag::General },
    SupplementEntry { syllable_key: "shen'shui'qu", text: "深水区", cost: 11.05, domain: DomainTag::General },
    SupplementEntry { syllable_key: "ban'gong'qu", text: "办公区", cost: 10.85, domain: DomainTag::General },
    SupplementEntry { syllable_key: "ju'min'qu", text: "居民区", cost: 10.85, domain: DomainTag::General },
    SupplementEntry { syllable_key: "shang'ye'qu", text: "商业区", cost: 10.85, domain: DomainTag::General },
    SupplementEntry { syllable_key: "he'xin'qu", text: "核心区", cost: 10.95, domain: DomainTag::General },
    SupplementEntry { syllable_key: "fu'wu'qu", text: "服务区", cost: 10.75, domain: DomainTag::General },
    SupplementEntry { syllable_key: "ge'li'qu", text: "隔离区", cost: 11.05, domain: DomainTag::General },
    SupplementEntry { syllable_key: "gong'jia'fan", text: "公家饭", cost: 10.85, domain: DomainTag::General },
    SupplementEntry { syllable_key: "chi'gong'jia'fan", text: "吃公家饭", cost: 10.65, domain: DomainTag::General },
    SupplementEntry { syllable_key: "chi'gong'jia'fan'de", text: "吃公家饭的", cost: 10.55, domain: DomainTag::General },
    SupplementEntry { syllable_key: "tie'fan'wan", text: "铁饭碗", cost: 10.75, domain: DomainTag::General },
    SupplementEntry { syllable_key: "ti'zhi'nei", text: "体制内", cost: 10.65, domain: DomainTag::General },
    SupplementEntry { syllable_key: "ti'zhi'wai", text: "体制外", cost: 10.85, domain: DomainTag::General },
    SupplementEntry { syllable_key: "chun'tang'ping", text: "纯躺平", cost: 10.75, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "ban'tang'ping", text: "半躺平", cost: 11.05, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "song'chi'gan", text: "松弛感", cost: 10.75, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "fen'wei'gan", text: "氛围感", cost: 10.65, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "bian'jie'gan", text: "边界感", cost: 10.75, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "po'sui'gan", text: "破碎感", cost: 10.95, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "gao'ji'gan", text: "高级感", cost: 10.75, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "ti'yan'gan", text: "体验感", cost: 10.85, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "yi'shi'gan", text: "仪式感", cost: 10.65, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "an'quan'gan", text: "安全感", cost: 10.45, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "cun'zai'gan", text: "存在感", cost: 10.55, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "qing'xu'jia'zhi", text: "情绪价值", cost: 10.55, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "jing'shen'nei'hao", text: "精神内耗", cost: 10.65, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "cao'tai'ban'zi", text: "草台班子", cost: 10.85, domain: DomainTag::Chat },
    SupplementEntry { syllable_key: "bi'li'bi'li", text: "哔哩哔哩", cost: 10.45, domain: DomainTag::Chat },

    // 5. 现代快递物流、电商履约与同城配送缩略复合词（补齐词素级积木如「标快/速运/冷运/次晨/驿站」，使四字组合自然涌现）
    SupplementEntry { syllable_key: "biao'kuai", text: "标快", cost: 10.95, domain: DomainTag::General },
    SupplementEntry { syllable_key: "su'yun", text: "速运", cost: 10.95, domain: DomainTag::General },
    SupplementEntry { syllable_key: "leng'yun", text: "冷运", cost: 11.15, domain: DomainTag::General },
    SupplementEntry { syllable_key: "ci'chen", text: "次晨", cost: 11.25, domain: DomainTag::General },
    SupplementEntry { syllable_key: "ji'fu", text: "寄付", cost: 11.15, domain: DomainTag::General },
    SupplementEntry { syllable_key: "bao'jia", text: "保价", cost: 11.05, domain: DomainTag::General },
    SupplementEntry { syllable_key: "yi'zhan", text: "驿站", cost: 10.85, domain: DomainTag::General },
    SupplementEntry { syllable_key: "feng'chao", text: "丰巢", cost: 10.95, domain: DomainTag::General },
    SupplementEntry { syllable_key: "feng'chao'gui", text: "丰巢柜", cost: 10.95, domain: DomainTag::General },
    SupplementEntry { syllable_key: "kuai'di'gui", text: "快递柜", cost: 10.65, domain: DomainTag::General },
    SupplementEntry { syllable_key: "qu'jian'ma", text: "取件码", cost: 10.55, domain: DomainTag::General },
    SupplementEntry { syllable_key: "ji'jian'ma", text: "寄件码", cost: 10.85, domain: DomainTag::General },
    SupplementEntry { syllable_key: "tong'cheng'ji'song", text: "同城急送", cost: 10.75, domain: DomainTag::General },
    SupplementEntry { syllable_key: "ji'tu", text: "极兔", cost: 11.05, domain: DomainTag::General },
];

/// 现代高频英文品牌、数码产品、主流平台、AI 生态与软硬件缩写补充词条。
///
/// 背景：远端 `merged_lexicon-2026.11.12.2.bin` 仅编入了雾凇拼音的普通单词表 `en.dict.yaml`（`google-10000-english`）
/// 与 150 条开发种子词，未纳入专有名词与互联网扩展表 `en_ext.dict.yaml`，导致 `iPhone`、`iPad`、`AirPods`、
/// `MacBook`、`WeChat`、`YouTube`、`Xcode`、`HDMI`、`OLED`、`URL` 等高频大小写专名缺失。
#[derive(Debug, Clone, Copy)]
pub struct EnglishSupplementEntry {
    pub text: &'static str,
    pub lower_key: &'static str,
    pub cost: f32,
    pub domain: DomainTag,
}

pub static ENGLISH_SUPPLEMENT_ENTRIES: &[EnglishSupplementEntry] = &[
    // 1. Apple 与主流移动/桌面软硬件生态
    EnglishSupplementEntry { text: "iPhone", lower_key: "iphone", cost: 9.05, domain: DomainTag::General },
    EnglishSupplementEntry { text: "iPad", lower_key: "ipad", cost: 9.15, domain: DomainTag::General },
    EnglishSupplementEntry { text: "AirPods", lower_key: "airpods", cost: 9.20, domain: DomainTag::General },
    EnglishSupplementEntry { text: "MacBook", lower_key: "macbook", cost: 9.18, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "iMac", lower_key: "imac", cost: 9.30, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "MacMini", lower_key: "macmini", cost: 9.55, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "MacStudio", lower_key: "macstudio", cost: 9.55, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "iCloud", lower_key: "icloud", cost: 9.25, domain: DomainTag::General },
    EnglishSupplementEntry { text: "AppleWatch", lower_key: "applewatch", cost: 9.35, domain: DomainTag::General },
    EnglishSupplementEntry { text: "iWatch", lower_key: "iwatch", cost: 9.45, domain: DomainTag::General },
    EnglishSupplementEntry { text: "iPod", lower_key: "ipod", cost: 9.50, domain: DomainTag::General },
    EnglishSupplementEntry { text: "HomePod", lower_key: "homepod", cost: 9.55, domain: DomainTag::General },
    EnglishSupplementEntry { text: "AirTag", lower_key: "airtag", cost: 9.55, domain: DomainTag::General },
    EnglishSupplementEntry { text: "AirDrop", lower_key: "airdrop", cost: 9.45, domain: DomainTag::General },
    EnglishSupplementEntry { text: "FaceID", lower_key: "faceid", cost: 9.45, domain: DomainTag::General },
    EnglishSupplementEntry { text: "TouchID", lower_key: "touchid", cost: 9.55, domain: DomainTag::General },
    EnglishSupplementEntry { text: "CarPlay", lower_key: "carplay", cost: 9.45, domain: DomainTag::General },
    EnglishSupplementEntry { text: "MagSafe", lower_key: "magsafe", cost: 9.55, domain: DomainTag::General },
    EnglishSupplementEntry { text: "iMessage", lower_key: "imessage", cost: 9.45, domain: DomainTag::General },
    EnglishSupplementEntry { text: "FaceTime", lower_key: "facetime", cost: 9.45, domain: DomainTag::General },
    EnglishSupplementEntry { text: "TestFlight", lower_key: "testflight", cost: 9.50, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "Xcode", lower_key: "xcode", cost: 9.30, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "SwiftUI", lower_key: "swiftui", cost: 9.30, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "Safari", lower_key: "safari", cost: 9.35, domain: DomainTag::General },
    EnglishSupplementEntry { text: "Siri", lower_key: "siri", cost: 9.40, domain: DomainTag::General },
    EnglishSupplementEntry { text: "ProMax", lower_key: "promax", cost: 9.55, domain: DomainTag::General },
    EnglishSupplementEntry { text: "eSIM", lower_key: "esim", cost: 9.50, domain: DomainTag::General },
    EnglishSupplementEntry { text: "eBook", lower_key: "ebook", cost: 9.60, domain: DomainTag::General },
    EnglishSupplementEntry { text: "HarmonyOS", lower_key: "harmonyos", cost: 9.45, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "ThinkPad", lower_key: "thinkpad", cost: 9.45, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "GeForce", lower_key: "geforce", cost: 9.50, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "Snapdragon", lower_key: "snapdragon", cost: 9.55, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "PlayStation", lower_key: "playstation", cost: 9.40, domain: DomainTag::General },
    EnglishSupplementEntry { text: "Xbox", lower_key: "xbox", cost: 9.45, domain: DomainTag::General },
    EnglishSupplementEntry { text: "Nintendo", lower_key: "nintendo", cost: 9.55, domain: DomainTag::General },
    EnglishSupplementEntry { text: "SteamDeck", lower_key: "steamdeck", cost: 9.60, domain: DomainTag::General },
    EnglishSupplementEntry { text: "Kindle", lower_key: "kindle", cost: 9.45, domain: DomainTag::General },

    // 2. 主流应用、社交内容平台与生产力软件
    EnglishSupplementEntry { text: "WeChat", lower_key: "wechat", cost: 9.25, domain: DomainTag::General },
    EnglishSupplementEntry { text: "TikTok", lower_key: "tiktok", cost: 9.30, domain: DomainTag::General },
    EnglishSupplementEntry { text: "YouTube", lower_key: "youtube", cost: 9.25, domain: DomainTag::General },
    EnglishSupplementEntry { text: "Bilibili", lower_key: "bilibili", cost: 10.85, domain: DomainTag::Chat },
    EnglishSupplementEntry { text: "Instagram", lower_key: "instagram", cost: 9.40, domain: DomainTag::General },
    EnglishSupplementEntry { text: "Facebook", lower_key: "facebook", cost: 9.40, domain: DomainTag::General },
    EnglishSupplementEntry { text: "Twitter", lower_key: "twitter", cost: 9.40, domain: DomainTag::General },
    EnglishSupplementEntry { text: "Telegram", lower_key: "telegram", cost: 9.40, domain: DomainTag::General },
    EnglishSupplementEntry { text: "WhatsApp", lower_key: "whatsapp", cost: 9.45, domain: DomainTag::General },
    EnglishSupplementEntry { text: "Discord", lower_key: "discord", cost: 9.40, domain: DomainTag::General },
    EnglishSupplementEntry { text: "Reddit", lower_key: "reddit", cost: 9.50, domain: DomainTag::General },
    EnglishSupplementEntry { text: "LinkedIn", lower_key: "linkedin", cost: 9.50, domain: DomainTag::General },
    EnglishSupplementEntry { text: "Netflix", lower_key: "netflix", cost: 9.45, domain: DomainTag::General },
    EnglishSupplementEntry { text: "Spotify", lower_key: "spotify", cost: 9.45, domain: DomainTag::General },
    EnglishSupplementEntry { text: "Steam", lower_key: "steam", cost: 9.40, domain: DomainTag::General },
    EnglishSupplementEntry { text: "Notion", lower_key: "notion", cost: 9.40, domain: DomainTag::General },
    EnglishSupplementEntry { text: "Obsidian", lower_key: "obsidian", cost: 9.45, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "Figma", lower_key: "figma", cost: 9.40, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "Canva", lower_key: "canva", cost: 9.50, domain: DomainTag::General },
    EnglishSupplementEntry { text: "Lightroom", lower_key: "lightroom", cost: 9.50, domain: DomainTag::General },
    EnglishSupplementEntry { text: "Premiere", lower_key: "premiere", cost: 9.55, domain: DomainTag::General },
    EnglishSupplementEntry { text: "AutoCAD", lower_key: "autocad", cost: 9.50, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "MATLAB", lower_key: "matlab", cost: 9.45, domain: DomainTag::Academic },
    EnglishSupplementEntry { text: "OneDrive", lower_key: "onedrive", cost: 9.50, domain: DomainTag::General },
    EnglishSupplementEntry { text: "Outlook", lower_key: "outlook", cost: 9.45, domain: DomainTag::General },
    EnglishSupplementEntry { text: "Keynote", lower_key: "keynote", cost: 9.50, domain: DomainTag::General },
    EnglishSupplementEntry { text: "Chrome", lower_key: "chrome", cost: 9.30, domain: DomainTag::General },
    EnglishSupplementEntry { text: "Firefox", lower_key: "firefox", cost: 9.45, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "Gmail", lower_key: "gmail", cost: 9.40, domain: DomainTag::General },
    EnglishSupplementEntry { text: "Google", lower_key: "google", cost: 9.20, domain: DomainTag::General },
    EnglishSupplementEntry { text: "Microsoft", lower_key: "microsoft", cost: 9.30, domain: DomainTag::General },
    EnglishSupplementEntry { text: "Amazon", lower_key: "amazon", cost: 9.40, domain: DomainTag::General },
    EnglishSupplementEntry { text: "Tesla", lower_key: "tesla", cost: 9.40, domain: DomainTag::General },
    EnglishSupplementEntry { text: "Samsung", lower_key: "samsung", cost: 9.45, domain: DomainTag::General },
    EnglishSupplementEntry { text: "Sony", lower_key: "sony", cost: 9.45, domain: DomainTag::General },
    EnglishSupplementEntry { text: "OPPO", lower_key: "oppo", cost: 9.40, domain: DomainTag::General },
    EnglishSupplementEntry { text: "vivo", lower_key: "vivo", cost: 9.40, domain: DomainTag::General },
    EnglishSupplementEntry { text: "DJI", lower_key: "dji", cost: 9.50, domain: DomainTag::General },
    EnglishSupplementEntry { text: "Nikon", lower_key: "nikon", cost: 9.55, domain: DomainTag::General },
    EnglishSupplementEntry { text: "Canon", lower_key: "canon", cost: 9.55, domain: DomainTag::General },
    EnglishSupplementEntry { text: "Leica", lower_key: "leica", cost: 9.55, domain: DomainTag::General },

    // 3. AI 大模型与现代技术栈专名
    EnglishSupplementEntry { text: "Copilot", lower_key: "copilot", cost: 9.35, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "Cursor", lower_key: "cursor", cost: 9.35, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "Midjourney", lower_key: "midjourney", cost: 9.45, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "Sora", lower_key: "sora", cost: 9.50, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "Perplexity", lower_key: "perplexity", cost: 9.55, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "Grok", lower_key: "grok", cost: 9.50, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "Llama", lower_key: "llama", cost: 9.45, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "Qwen", lower_key: "qwen", cost: 9.35, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "Kimi", lower_key: "kimi", cost: 9.40, domain: DomainTag::General },
    EnglishSupplementEntry { text: "HuggingFace", lower_key: "huggingface", cost: 9.50, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "LangChain", lower_key: "langchain", cost: 9.55, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "Vulkan", lower_key: "vulkan", cost: 9.50, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "CoreML", lower_key: "coreml", cost: 9.50, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "WebGL", lower_key: "webgl", cost: 9.50, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "WebGPU", lower_key: "webgpu", cost: 9.50, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "Kotlin", lower_key: "kotlin", cost: 9.35, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "Flutter", lower_key: "flutter", cost: 9.35, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "Golang", lower_key: "golang", cost: 9.35, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "NodeJS", lower_key: "nodejs", cost: 9.40, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "NextJS", lower_key: "nextjs", cost: 9.45, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "VueJS", lower_key: "vuejs", cost: 9.50, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "Tailwind", lower_key: "tailwind", cost: 9.50, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "Xcodeproj", lower_key: "xcodeproj", cost: 9.65, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "UniFFI", lower_key: "uniffi", cost: 9.65, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "Vercel", lower_key: "vercel", cost: 9.55, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "Supabase", lower_key: "supabase", cost: 9.55, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "ArkTS", lower_key: "arkts", cost: 9.55, domain: DomainTag::Tech },

    // 4. 硬件接口、显示、网络协议与常见文件格式缩写
    //    （与高频中文简拼重叠的 3 字母缩写如 SSH/SSD/LCD/LED 定在 11.4，保持中文首选并通过黄金第二位呈现）
    EnglishSupplementEntry { text: "URL", lower_key: "url", cost: 9.25, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "USB", lower_key: "usb", cost: 9.20, domain: DomainTag::General },
    EnglishSupplementEntry { text: "TypeC", lower_key: "typec", cost: 9.25, domain: DomainTag::General },
    EnglishSupplementEntry { text: "HDMI", lower_key: "hdmi", cost: 9.25, domain: DomainTag::General },
    EnglishSupplementEntry { text: "OLED", lower_key: "oled", cost: 9.25, domain: DomainTag::General },
    EnglishSupplementEntry { text: "AMOLED", lower_key: "amoled", cost: 9.40, domain: DomainTag::General },
    EnglishSupplementEntry { text: "QLED", lower_key: "qled", cost: 9.50, domain: DomainTag::General },
    EnglishSupplementEntry { text: "HDR", lower_key: "hdr", cost: 9.45, domain: DomainTag::General },
    EnglishSupplementEntry { text: "RGB", lower_key: "rgb", cost: 9.45, domain: DomainTag::General },
    EnglishSupplementEntry { text: "NFC", lower_key: "nfc", cost: 9.40, domain: DomainTag::General },
    EnglishSupplementEntry { text: "GPS", lower_key: "gps", cost: 9.30, domain: DomainTag::General },
    EnglishSupplementEntry { text: "VPN", lower_key: "vpn", cost: 9.30, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "DNS", lower_key: "dns", cost: 9.40, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "DHCP", lower_key: "dhcp", cost: 9.55, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "TCP", lower_key: "tcp", cost: 9.45, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "UDP", lower_key: "udp", cost: 9.45, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "IPv4", lower_key: "ipv4", cost: 9.45, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "IPv6", lower_key: "ipv6", cost: 9.45, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "BIOS", lower_key: "bios", cost: 9.40, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "UEFI", lower_key: "uefi", cost: 9.45, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "NVMe", lower_key: "nvme", cost: 9.45, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "PCIe", lower_key: "pcie", cost: 9.45, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "VRAM", lower_key: "vram", cost: 9.45, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "DRAM", lower_key: "dram", cost: 9.50, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "DDR5", lower_key: "ddr5", cost: 9.50, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "EUV", lower_key: "euv", cost: 9.50, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "DUV", lower_key: "duv", cost: 9.55, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "ASML", lower_key: "asml", cost: 9.50, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "APK", lower_key: "apk", cost: 9.35, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "IPA", lower_key: "ipa", cost: 9.45, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "HEIC", lower_key: "heic", cost: 9.50, domain: DomainTag::General },
    EnglishSupplementEntry { text: "WEBP", lower_key: "webp", cost: 9.50, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "JPEG", lower_key: "jpeg", cost: 9.40, domain: DomainTag::General },
    EnglishSupplementEntry { text: "FLAC", lower_key: "flac", cost: 9.45, domain: DomainTag::General },
    EnglishSupplementEntry { text: "MP3", lower_key: "mp3", cost: 9.25, domain: DomainTag::General },
    EnglishSupplementEntry { text: "MP4", lower_key: "mp4", cost: 9.20, domain: DomainTag::General },
    EnglishSupplementEntry { text: "YAML", lower_key: "yaml", cost: 9.40, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "TOML", lower_key: "toml", cost: 9.45, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "UTF8", lower_key: "utf8", cost: 9.45, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "CSV", lower_key: "csv", cost: 9.45, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "SSH", lower_key: "ssh", cost: 13.50, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "SSD", lower_key: "ssd", cost: 13.80, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "LCD", lower_key: "lcd", cost: 13.20, domain: DomainTag::General },
    EnglishSupplementEntry { text: "LED", lower_key: "led", cost: 13.20, domain: DomainTag::General },
    EnglishSupplementEntry { text: "FPS", lower_key: "fps", cost: 11.20, domain: DomainTag::General },
    EnglishSupplementEntry { text: "PNG", lower_key: "png", cost: 11.20, domain: DomainTag::General },
    EnglishSupplementEntry { text: "JPG", lower_key: "jpg", cost: 11.20, domain: DomainTag::General },
    EnglishSupplementEntry { text: "GIF", lower_key: "gif", cost: 10.60, domain: DomainTag::General },
    EnglishSupplementEntry { text: "SVG", lower_key: "svg", cost: 10.80, domain: DomainTag::Tech },
    EnglishSupplementEntry { text: "XML", lower_key: "xml", cost: 11.20, domain: DomainTag::Tech },
];

struct SupplementIndex {
    syllable_trie: FlatTrie,
    abbrev_trie: FlatTrie,
    english_trie: FlatTrie,
}

static SUPPLEMENT_INDEX: LazyLock<SupplementIndex> = LazyLock::new(|| {
    let mut order: Vec<usize> = (0..SUPPLEMENT_ENTRIES.len()).collect();
    order.sort_by(|&a, &b| {
        SUPPLEMENT_ENTRIES[a]
            .cost
            .total_cmp(&SUPPLEMENT_ENTRIES[b].cost)
            .then_with(|| SUPPLEMENT_ENTRIES[a].text.cmp(SUPPLEMENT_ENTRIES[b].text))
    });
    let mut syllable_trie = FlatTrie::new();
    let mut abbrev_trie = FlatTrie::new();
    for idx in order {
        let entry = &SUPPLEMENT_ENTRIES[idx];
        syllable_trie.insert_with_limit(entry.syllable_key, idx as u32, true, 6, 16);
        let abbrev: String = entry
            .syllable_key
            .split('\'')
            .filter_map(|s| s.chars().next())
            .collect();
        if abbrev.len() >= 2 {
            abbrev_trie.insert_with_limit(&abbrev, idx as u32, false, 0, 16);
        }
    }

    let mut eng_order: Vec<usize> = (0..ENGLISH_SUPPLEMENT_ENTRIES.len()).collect();
    eng_order.sort_by(|&a, &b| {
        ENGLISH_SUPPLEMENT_ENTRIES[a]
            .cost
            .total_cmp(&ENGLISH_SUPPLEMENT_ENTRIES[b].cost)
            .then_with(|| {
                ENGLISH_SUPPLEMENT_ENTRIES[a]
                    .text
                    .cmp(ENGLISH_SUPPLEMENT_ENTRIES[b].text)
            })
    });
    let mut english_trie = FlatTrie::new();
    for idx in eng_order {
        let entry = &ENGLISH_SUPPLEMENT_ENTRIES[idx];
        english_trie.insert_with_limit(entry.lower_key, idx as u32, true, 8, 8);
    }

    SupplementIndex {
        syllable_trie,
        abbrev_trie,
        english_trie,
    }
});

#[inline]
pub fn english_supplement_trie() -> &'static FlatTrie {
    &SUPPLEMENT_INDEX.english_trie
}

#[inline]
pub fn english_supplement_entry(id: u32) -> &'static EnglishSupplementEntry {
    &ENGLISH_SUPPLEMENT_ENTRIES[id as usize]
}

#[inline]
pub fn has_english_entry(lexicon: &ime_lexicon::CompiledLexicon, lower_key: &str) -> bool {
    !lexicon.english_trie().exact_lookup(lower_key).is_empty()
        || !SUPPLEMENT_INDEX
            .english_trie
            .exact_lookup(lower_key)
            .is_empty()
}

#[inline]
pub fn has_english_prefix(lexicon: &ime_lexicon::CompiledLexicon, lower_suffix: &str) -> bool {
    lexicon
        .english_trie()
        .common_prefix_search(lower_suffix)
        .next()
        .is_some()
        || !SUPPLEMENT_INDEX
            .english_trie
            .common_prefix_search(lower_suffix)
            .is_empty()
}

/// 查询主词库与补充词表中覆盖 `lower_key` 的最优（最低 cost）规范大小写英文词文本
pub fn best_english_exact_text<'a>(
    lexicon: &'a ime_lexicon::CompiledLexicon,
    lower_key: &str,
) -> Option<&'a str> {
    let lex_best = lexicon
        .english_trie()
        .exact_lookup(lower_key)
        .first()
        .map(|id| {
            let e = lexicon.entry(id);
            (e.cost(), e.text())
        });
    let sup_best = SUPPLEMENT_INDEX
        .english_trie
        .exact_lookup(lower_key)
        .first()
        .map(|&id| {
            let e = &ENGLISH_SUPPLEMENT_ENTRIES[id as usize];
            (e.cost, e.text)
        });
    match (lex_best, sup_best) {
        (Some((lc, lt)), Some((sc, st))) => {
            if sc < lc {
                Some(st)
            } else {
                Some(lt)
            }
        }
        (Some((_, lt)), None) => Some(lt),
        (None, Some((_, st))) => Some(st),
        (None, None) => None,
    }
}

/// 收集补充词表的全拼/模糊音及末尾不完整音节边
pub fn collect_supplement_edges(
    origin_start: usize,
    input: &str,
    spans_at: &[Vec<&SyllableSpan>],
    out_edges: &mut Vec<LatticeEdge>,
) {
    collect_dfs(
        origin_start,
        origin_start,
        0,
        0,
        0.0,
        input,
        spans_at,
        out_edges,
    );
}

#[allow(clippy::too_many_arguments)]
fn collect_dfs(
    origin_start: usize,
    pos: usize,
    curr_node: u32,
    depth: usize,
    accum_fuzzy: f32,
    input: &str,
    spans_at: &[Vec<&SyllableSpan>],
    out_edges: &mut Vec<LatticeEdge>,
) {
    let n = input.len();
    if depth >= 6 || pos >= n {
        return;
    }
    let mut effective_pos = pos;
    if depth > 0 {
        while effective_pos < n && input.as_bytes()[effective_pos] == b'\'' {
            effective_pos += 1;
        }
        if effective_pos >= n {
            return;
        }
    }
    let trie = &SUPPLEMENT_INDEX.syllable_trie;
    let base_node = if depth == 0 {
        curr_node
    } else {
        match trie.advance_node(curr_node, "'") {
            Some(nd) => nd,
            None => return,
        }
    };
    let syl_count = depth + 1;
    for &sp in &spans_at[effective_pos] {
        let new_fuzzy = accum_fuzzy + sp.fuzzy_penalty;
        if new_fuzzy > 3.6 {
            continue;
        }
        let Some(next_node) = trie.advance_node(base_node, &sp.syllable) else {
            continue;
        };
        let hit_ids = &trie.nodes[next_node as usize].exact_entries;
        if !hit_ids.is_empty() {
            let raw_span = &input[origin_start..sp.end];
            let splits_single_syl = syl_count >= 2 && is_unapostrophed_single_syllable(raw_span);
            if !(splits_single_syl && new_fuzzy > 0.0) {
                let split_pen = if splits_single_syl { 0.70 } else { 0.0 };
                let fuzzy_pen = if new_fuzzy > 0.0 { new_fuzzy * 1.10 } else { 0.0 };
                for &id in hit_ids {
                    let entry = &SUPPLEMENT_ENTRIES[id as usize];
                    let target_cost = entry.cost + fuzzy_pen + split_pen;
                    if let Some(existing) = out_edges.iter_mut().find(|e| {
                        e.start == origin_start
                            && e.end == sp.end
                            && e.text == entry.text
                            && e.is_fuzzy == (new_fuzzy > 0.0)
                    }) {
                        if target_cost < existing.base_cost {
                            existing.base_cost = target_cost;
                        }
                        continue;
                    }
                    out_edges.push(LatticeEdge {
                        start: origin_start,
                        end: sp.end,
                        text: entry.text.to_string(),
                        code_key: entry.syllable_key.to_string(),
                        kind: EdgeKind::ChineseWord,
                        base_cost: target_cost,
                        domain: entry.domain,
                        syllable_count: syl_count,
                        is_fuzzy: new_fuzzy > 0.0,
                        pronunciation_hint: None,
                    });
                }
            }
        }
        collect_dfs(
            origin_start,
            sp.end,
            next_node,
            syl_count,
            new_fuzzy,
            input,
            spans_at,
            out_edges,
        );
    }

    // 末尾不完整音节补全（如 `gongh` -> `公户`、`jiew` -> `接娃`、`furenq` -> `富人区`）
    let whole_slice_lower = input[origin_start..n].to_ascii_lowercase();
    if depth >= 1
        && accum_fuzzy == 0.0
        && effective_pos < n
        && !crate::pinyin::is_valid_or_alias_syllable(&whole_slice_lower)
    {
        let rem_tail = &input[effective_pos..n];
        if rem_tail.len() <= 4
            && rem_tail.bytes().all(|b| b.is_ascii_lowercase())
            && !crate::pinyin::is_valid_or_alias_syllable(rem_tail)
        {
            if let Some(p_node) = trie.advance_node(base_node, rem_tail) {
                for &id in trie.nodes[p_node as usize].top_prefix_entries.iter().take(4) {
                    let entry = &SUPPLEMENT_ENTRIES[id as usize];
                    if entry.syllable_key.split('\'').count() != syl_count {
                        continue;
                    }
                    if out_edges
                        .iter()
                        .any(|e| e.start == origin_start && e.end == n && e.text == entry.text)
                    {
                        continue;
                    }
                    out_edges.push(LatticeEdge {
                        start: origin_start,
                        end: n,
                        text: entry.text.to_string(),
                        code_key: entry.syllable_key.to_string(),
                        kind: EdgeKind::ChineseWord,
                        base_cost: entry.cost + 0.95,
                        domain: entry.domain,
                        syllable_count: syl_count,
                        is_fuzzy: false,
                        pronunciation_hint: None,
                    });
                }
            }
        }
    }
}

/// 补充词表简拼精确匹配（仅用于整串非全拼的纯简拼查询，如 `gh -> 公户`、`shkh -> 私行客户`）
pub fn lookup_supplement_abbrev(abbrev: &str) -> Vec<&'static SupplementEntry> {
    SUPPLEMENT_INDEX
        .abbrev_trie
        .exact_lookup(abbrev)
        .iter()
        .map(|&id| &SUPPLEMENT_ENTRIES[id as usize])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supplement_entries_are_valid_bmp_and_syllables() {
        for e in SUPPLEMENT_ENTRIES {
            assert!(!e.text.is_empty());
            assert!(
                e.text.chars().all(|c| (c as u32) <= 0xFFFF),
                "non-BMP char in {}",
                e.text
            );
            let syls: Vec<&str> = e.syllable_key.split('\'').collect();
            assert_eq!(
                syls.len(),
                e.text.chars().count(),
                "syllable count mismatch in {}",
                e.text
            );
            for s in syls {
                assert!(
                    crate::pinyin::is_valid_syllable(s),
                    "invalid syllable {s} in {}",
                    e.text
                );
            }
        }
        assert!(lookup_supplement_abbrev("shkh")
            .iter()
            .any(|e| e.text == "私行客户"));

        for eng in ENGLISH_SUPPLEMENT_ENTRIES {
            assert!(!eng.text.is_empty());
            assert!(eng.text.is_ascii(), "non-ASCII in {}", eng.text);
            assert_eq!(
                eng.lower_key,
                eng.text.to_ascii_lowercase(),
                "lower_key mismatch for {}",
                eng.text
            );
            assert!(
                eng.lower_key.bytes().all(|b| b.is_ascii_alphanumeric()),
                "invalid lower_key {}",
                eng.lower_key
            );
            assert!((8.5..=14.5).contains(&eng.cost), "cost out of range for {}", eng.text);
        }
        assert_eq!(
            ENGLISH_SUPPLEMENT_ENTRIES[english_supplement_trie().exact_lookup("iphone")[0] as usize].text,
            "iPhone"
        );
    }
}
