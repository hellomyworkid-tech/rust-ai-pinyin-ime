//! `corrector`: 常见易读错字词召回与正确读音标注（参考雾凇拼音 `rime-ice` `corrector.lua` 与现代汉语规范读音）
//!
//! 设计目标：
//! 1. 用户按大众常见误读输入时（如 `hundun` -> `馄饨`、`jiaose` -> `角色`、`geiyu` -> `给予`、
//!    `fazhi` -> `阈值`、`moban` -> `模板`、`youhuijuan` -> `优惠券`），无需重编远端词库即可毫秒级召回目标词；
//! 2. 当存在常用正音同音词时（如 `hundun` 的「混沌」），保持正音词排第 1，易错音词紧随其后，
//!    并附带正确读音提示（如 `"馄 (hun) 饨 (tun)"`），仅用于候选栏展示，上屏文本仍只取词条本身；
//! 3. 用户输入正确读音时（如 `huntun` -> `馄饨`、`juese` -> `角色`）不走本表，不显示冗余读音提示。

use crate::lattice::{EdgeKind, LatticeEdge};
use crate::pinyin::{is_unapostrophed_single_syllable, SyllableSpan};
use ime_lexicon::{DomainTag, FlatTrie};
use std::collections::HashMap;
use std::sync::LazyLock;

#[derive(Debug, Clone, Copy)]
pub struct MisreadEntry {
    pub misread_key: &'static str,
    pub text: &'static str,
    pub correct_key: &'static str,
    pub hint: &'static str,
    pub base_cost: f32,
    pub near_synonym: bool,
}

pub static MISREAD_ENTRIES: &[MisreadEntry] = &[
    MisreadEntry { misread_key: "an'nai", text: "按捺", correct_key: "an'na", hint: "按 (an) 捺 (na)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "an'nai'bu'zhu", text: "按捺不住", correct_key: "an'na'bu'zhu", hint: "捺 (na)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "bi'mei", text: "媲美", correct_key: "pi'mei", hint: "媲 (pi) 美 (mei)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "bu'gei", text: "补给", correct_key: "bu'ji", hint: "补 (bu) 给 (ji)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "bu'gei'pin", text: "补给品", correct_key: "bu'ji'pin", hint: "给 (ji)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "cheng'zhi", text: "称职", correct_key: "chen'zhi", hint: "称 (chen) 职 (zhi)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "chi'hun'dun", text: "吃馄饨", correct_key: "chi'hun'tun", hint: "饨 (tun)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "cu'zao", text: "粗糙", correct_key: "cu'cao", hint: "粗 (cu) 糙 (cao)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "dai'jin'juan", text: "代金券", correct_key: "dai'jin'quan", hint: "券 (quan)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "dui'huan'juan", text: "兑换券", correct_key: "dui'huan'quan", hint: "券 (quan)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "fa'xiao", text: "发酵", correct_key: "fa'jiao", hint: "发 (fa) 酵 (jiao)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "fa'zhi", text: "阈值", correct_key: "yu'zhi", hint: "阈 (yu) 值 (zhi)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "gei'yu", text: "给予", correct_key: "ji'yu", hint: "给 (ji) 予 (yu)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "gong'gei", text: "供给", correct_key: "gong'ji", hint: "供 (gong) 给 (ji)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "gong'gei'ce", text: "供给侧", correct_key: "gong'ji'ce", hint: "给 (ji)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "guan'ka", text: "关卡", correct_key: "guan'qia", hint: "关 (guan) 卡 (qia)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "hong'pei", text: "烘焙", correct_key: "hong'bei", hint: "烘 (hong) 焙 (bei)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "hun'dun", text: "馄饨", correct_key: "hun'tun", hint: "馄 (hun) 饨 (tun)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "hun'dun'mian", text: "馄饨面", correct_key: "hun'tun'mian", hint: "饨 (tun)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "jiao'se", text: "角色", correct_key: "jue'se", hint: "角 (jue) 色 (se)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "jie'pao", text: "解剖", correct_key: "jie'pou", hint: "解 (jie) 剖 (pou)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "ka'bo'zi", text: "卡脖子", correct_key: "qia'bo'zi", hint: "卡 (qia)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "luo'shi'fen", text: "螺蛳粉", correct_key: "luo'si'fen", hint: "蛳 (si)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "ma'pi", text: "麻痹", correct_key: "ma'bi", hint: "麻 (ma) 痹 (bi)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "mai'yuan", text: "埋怨", correct_key: "man'yuan", hint: "埋 (man) 怨 (yuan)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "mo'ban", text: "模板", correct_key: "mu'ban", hint: "模 (mu) 板 (ban)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "mo'ju", text: "模具", correct_key: "mu'ju", hint: "模 (mu) 具 (ju)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "mo'xie'zuo", text: "摩羯座", correct_key: "mo'jie'zuo", hint: "羯 (jie)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "mo'yang", text: "模样", correct_key: "mu'yang", hint: "模 (mu) 样 (yang)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "nian'tie", text: "粘贴", correct_key: "zhan'tie", hint: "粘 (zhan) 贴 (tie)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "pao'xi", text: "剖析", correct_key: "pou'xi", hint: "剖 (pou) 析 (xi)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "pei'gei", text: "配给", correct_key: "pei'ji", hint: "配 (pei) 给 (ji)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "pu'ru", text: "哺乳", correct_key: "bu'ru", hint: "哺 (bu) 乳 (ru)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "qiang'jin", text: "强劲", correct_key: "qiang'jing", hint: "强 (qiang) 劲 (jing)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "ru'chang'juan", text: "入场券", correct_key: "ru'chang'quan", hint: "券 (quan)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "shui'fu", text: "说服", correct_key: "shuo'fu", hint: "说 (shuo) 服 (fu)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "ti'yan'juan", text: "体验券", correct_key: "ti'yan'quan", hint: "券 (quan)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "tong'jing", text: "憧憬", correct_key: "chong'jing", hint: "憧 (chong) 憬 (jing)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "xian'rou'hun'dun", text: "鲜肉馄饨", correct_key: "xian'rou'hun'tun", hint: "饨 (tun)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "xiao'fei'juan", text: "消费券", correct_key: "xiao'fei'quan", hint: "券 (quan)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "xiao'hun'dun", text: "小馄饨", correct_key: "xiao'hun'tun", hint: "饨 (tun)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "xiao'mu", text: "酵母", correct_key: "jiao'mu", hint: "酵 (jiao) 母 (mu)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "yi'jiu'er'jiu", text: "一蹴而就", correct_key: "yi'cu'er'jiu", hint: "蹴 (cu)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "you'hui'juan", text: "优惠券", correct_key: "you'hui'quan", hint: "券 (quan)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "zao'zhi", text: "造诣", correct_key: "zao'yi", hint: "造 (zao) 诣 (yi)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "zhai'juan", text: "债券", correct_key: "zhai'quan", hint: "债 (zhai) 券 (quan)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "zheng'juan", text: "证券", correct_key: "zheng'quan", hint: "证 (zheng) 券 (quan)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "zhu'jiao", text: "主角", correct_key: "zhu'jue", hint: "主 (zhu) 角 (jue)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "zi'gei'zi'zu", text: "自给自足", correct_key: "zi'ji'zi'zu", hint: "给 (ji)", base_cost: 10.60, near_synonym: false },
    MisreadEntry { misread_key: "a'mi'tuo'fo", text: "阿弥陀佛", correct_key: "e'mi'tuo'fo", hint: "阿 (e)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "an'hui'liu'an", text: "安徽六安", correct_key: "an'hui'lu'an", hint: "六 (lu)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "an'hui'liu'an'shi", text: "安徽六安市", correct_key: "an'hui'lu'an'shi", hint: "六 (lu)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "an'hui'sheng'liu'an'shi", text: "安徽省六安市", correct_key: "an'hui'sheng'lu'an'shi", hint: "六 (lu)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "an'pou", text: "安瓿", correct_key: "an'bu", hint: "安 (an) 瓿 (bu)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "an'pou'ping", text: "安瓿瓶", correct_key: "an'bu'ping", hint: "瓿 (bu)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "bai'zhe'bu'rao", text: "百折不挠", correct_key: "bai'zhe'bu'nao", hint: "挠 (nao)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "bang'bu", text: "蚌埠", correct_key: "beng'bu", hint: "蚌 (beng) 埠 (bu)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "bao'lian'tian'wu", text: "暴殄天物", correct_key: "bao'tian'tian'wu", hint: "殄 (tian)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "ben'lin", text: "贲临", correct_key: "bi'lin", hint: "贲 (bi) 临 (lin)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "bi'ji", text: "荸荠", correct_key: "bi'qi", hint: "荸 (bi) 荠 (qi)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "bian'da", text: "鞭挞", correct_key: "bian'ta", hint: "鞭 (bian) 挞 (ta)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "bian'tai", text: "鞭笞", correct_key: "bian'chi", hint: "鞭 (bian) 笞 (chi)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "biao'han", text: "剽悍", correct_key: "piao'han", hint: "剽 (piao) 悍 (han)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "bin'lang", text: "槟榔", correct_key: "bing'lang", hint: "槟 (bing) 榔 (lang)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "bo'jing", text: "脖颈", correct_key: "bo'geng", hint: "脖 (bo) 颈 (geng)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "bo'jing'er", text: "脖颈儿", correct_key: "bo'geng'er", hint: "颈 (geng)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "bo'wen'qiang'shi", text: "博闻强识", correct_key: "bo'wen'qiang'zhi", hint: "识 (zhi)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "bo'yang'hu", text: "鄱阳湖", correct_key: "po'yang'hu", hint: "鄱 (po)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "bu'di", text: "不啻", correct_key: "bu'chi", hint: "不 (bu) 啻 (chi)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "can'he", text: "掺和", correct_key: "chan'huo", hint: "掺 (chan) 和 (huo)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "can'huo", text: "掺和", correct_key: "chan'huo", hint: "掺 (chan) 和 (huo)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "cao'guan'ren'ming", text: "草菅人命", correct_key: "cao'jian'ren'ming", hint: "菅 (jian)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "cao'zhi", text: "草薙", correct_key: "cao'ti", hint: "草 (cao) 薙 (ti)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "cao'zhi'jian", text: "草薙剑", correct_key: "cao'ti'jian", hint: "薙 (ti)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "cao'zhi'jing", text: "草薙京", correct_key: "cao'ti'jing", hint: "薙 (ti)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "chan'he", text: "掺和", correct_key: "chan'huo", hint: "掺 (chan) 和 (huo)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "chi'pi'sa", text: "吃比萨", correct_key: "chi'bi'sa", hint: "比 (bi)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "chui'shou'ke'de", text: "唾手可得", correct_key: "tuo'shou'ke'de", hint: "唾 (tuo)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "ci'ya", text: "龇牙", correct_key: "zi'ya", hint: "龇 (zi) 牙 (ya)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "ci'ya'lie'zui", text: "龇牙咧嘴", correct_key: "zi'ya'lie'zui", hint: "龇 (zi)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "ci'zhe'ya", text: "龇着牙", correct_key: "zi'zhe'ya", hint: "龇 (zi)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "cu'kuang", text: "粗犷", correct_key: "cu'guang", hint: "粗 (cu) 犷 (guang)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "da'fu'bian'bian", text: "大腹便便", correct_key: "da'fu'pian'pian", hint: "便 (pian)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "da'wan'ma", text: "大宛马", correct_key: "da'yuan'ma", hint: "宛 (yuan)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "dao'hang", text: "道行", correct_key: "dao'heng", hint: "道 (dao) 行 (heng)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "dao'shang", text: "捯饬", correct_key: "dao'chi", hint: "捯 (dao) 饬 (chi)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "dao'zai'shi'ni", text: "道在屎溺", correct_key: "dao'zai'shi'niao", hint: "溺 (niao)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "di'hu'guan'ding", text: "醍醐灌顶", correct_key: "ti'hu'guan'ding", hint: "醍 (ti)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "du'le'le", text: "独乐乐", correct_key: "du'yue'le", hint: "乐 (yue)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "e'fu", text: "饿殍", correct_key: "e'piao", hint: "饿 (e) 殍 (piao)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "feng'chan", text: "封禅", correct_key: "feng'shan", hint: "封 (feng) 禅 (shan)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "fu'gu'zhi'qu", text: "附骨之疽", correct_key: "fu'gu'zhi'ju", hint: "疽 (ju)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "ga'li", text: "蛤蜊", correct_key: "ge'li", hint: "蛤 (ge) 蜊 (li)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "ga'ran'er'zhi", text: "戛然而止", correct_key: "jia'ran'er'zhi", hint: "戛 (jia)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "gang'fu'zi'yong", text: "刚愎自用", correct_key: "gang'bi'zi'yong", hint: "愎 (bi)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "gao'ju'li", text: "高句丽", correct_key: "gao'gou'li", hint: "句 (gou)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "ge'jin'zhi'shou", text: "恪尽职守", correct_key: "ke'jin'zhi'shou", hint: "恪 (ke)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "ge'shou", text: "恪守", correct_key: "ke'shou", hint: "恪 (ke) 守 (shou)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "ge'zhi'wo", text: "胳肢窝", correct_key: "ga'zhi'wo", hint: "胳 (ga)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "gong'quan", text: "拱券", correct_key: "gong'xuan", hint: "拱 (gong) 券 (xuan)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "gong'shang'jiao'zhi'yu", text: "宫商角徵羽", correct_key: "gong'shang'jue'zhi'yu", hint: "角 (jue)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "gu'xian", text: "股癣", correct_key: "gu'xuan", hint: "股 (gu) 癣 (xuan)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "gua'gua'zhui'di", text: "呱呱坠地", correct_key: "gu'gu'zhui'di", hint: "呱 (gu)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "gui'bu", text: "跬步", correct_key: "kui'bu", hint: "跬 (kui) 步 (bu)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "gui'ju", text: "诡谲", correct_key: "gui'jue", hint: "诡 (gui) 谲 (jue)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "gui'lie", text: "龟裂", correct_key: "jun'lie", hint: "龟 (jun) 裂 (lie)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "guo'pu", text: "果脯", correct_key: "guo'fu", hint: "果 (guo) 脯 (fu)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "hai'shen'wei", text: "海参崴", correct_key: "hai'shen'wai", hint: "崴 (wai)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "hai'shi'chen'lou", text: "海市蜃楼", correct_key: "hai'shi'shen'lou", hint: "蜃 (shen)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "hao'zhou", text: "亳州", correct_key: "bo'zhou", hint: "亳 (bo) 州 (zhou)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "hou'pu", text: "厚朴", correct_key: "hou'po", hint: "厚 (hou) 朴 (po)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "hua'ban'xian", text: "花斑癣", correct_key: "hua'ban'xuan", hint: "癣 (xuan)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "huo'luo", text: "饸饹", correct_key: "he'le", hint: "饸 (he) 饹 (le)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "jia'ping'ao", text: "贾平凹", correct_key: "jia'ping'wa", hint: "凹 (wa)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "jiang'juan", text: "奖券", correct_key: "jiang'quan", hint: "奖 (jiang) 券 (quan)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "jiao'dou'shi", text: "角斗士", correct_key: "jue'dou'shi", hint: "角 (jue)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "jiao'xian", text: "脚癣", correct_key: "jiao'xuan", hint: "脚 (jiao) 癣 (xuan)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "jiao'zhu", text: "角逐", correct_key: "jue'zhu", hint: "角 (jue) 逐 (zhu)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "jie'si'di'li", text: "歇斯底里", correct_key: "xie'si'di'li", hint: "歇 (xie)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "jie'zha", text: "结扎", correct_key: "jie'za", hint: "结 (jie) 扎 (za)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "jin'kui", text: "金匮", correct_key: "jin'gui", hint: "金 (jin) 匮 (gui)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "jing'gao", text: "甑糕", correct_key: "zeng'gao", hint: "甑 (zeng) 糕 (gao)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "ju'mai'cai", text: "苣荬菜", correct_key: "qu'mai'cai", hint: "苣 (qu)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "ju'tou", text: "噱头", correct_key: "xue'tou", hint: "噱 (xue) 头 (tou)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "li'li", text: "李悝", correct_key: "li'kui", hint: "李 (li) 悝 (kui)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "li'neng'kang'ding", text: "力能扛鼎", correct_key: "li'neng'gang'ding", hint: "扛 (gang)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "lian'jue", text: "联袂", correct_key: "lian'mei", hint: "联 (lian) 袂 (mei)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "liu'an'shi", text: "六安市", correct_key: "lu'an'shi", hint: "六 (lu)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "liu'an'xian", text: "六安县", correct_key: "lu'an'xian", hint: "六 (lu)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "luo'an'suan", text: "酪氨酸", correct_key: "lao'an'suan", hint: "酪 (lao)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "mei'shi'zi", text: "没食子", correct_key: "mo'shi'zi", hint: "没 (mo)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "mi'lu", text: "秘鲁", correct_key: "bi'lu", hint: "秘 (bi) 鲁 (lu)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "mian'mian'xiang'xu", text: "面面相觑", correct_key: "mian'mian'xiang'qu", hint: "觑 (qu)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "miao'lun", text: "谬论", correct_key: "miu'lun", hint: "谬 (miu) 论 (lun)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "mo'ling'liang'ke", text: "模棱两可", correct_key: "mo'leng'liang'ke", hint: "棱 (leng)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "mu'na", text: "木讷", correct_key: "mu'ne", hint: "木 (mu) 讷 (ne)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "na'mo'a'mi'tuo'fo", text: "南无阿弥陀佛", correct_key: "na'mo'e'mi'tuo'fo", hint: "阿 (e)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "nan'jing'liu'he", text: "南京六合", correct_key: "nan'jing'lu'he", hint: "六 (lu)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "nan'jing'shi'liu'he", text: "南京市六合", correct_key: "nan'jing'shi'lu'he", hint: "六 (lu)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "nan'jing'shi'liu'he'qu", text: "南京市六合区", correct_key: "nan'jing'shi'lu'he'qu", hint: "六 (lu)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "nan'wu'a'mi'tuo'fo", text: "南无阿弥陀佛", correct_key: "na'mo'e'mi'tuo'fo", hint: "南 (na) 无 (mo) 阿 (e)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "nan'wu'e'mi'tuo'fo", text: "南无阿弥陀佛", correct_key: "na'mo'e'mi'tuo'fo", hint: "南 (na) 无 (mo)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "nao'gu", text: "桡骨", correct_key: "rao'gu", hint: "桡 (rao) 骨 (gu)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "nao'zu'zhong", text: "脑卒中", correct_key: "nao'cu'zhong", hint: "卒 (cu)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "nian'yi'nian", text: "粘一粘", correct_key: "zhan'yi'zhan", hint: "粘 (zhan)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "niu'jing", text: "牛丼", correct_key: "niu'dan", hint: "牛 (niu) 丼 (dan)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "niu'jing'fan", text: "牛丼饭", correct_key: "niu'dan'fan", hint: "丼 (dan)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "niu'kou", text: "拗口", correct_key: "ao'kou", hint: "拗 (ao) 口 (kou)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "niu'pi'xian", text: "牛皮癣", correct_key: "niu'pi'xuan", hint: "癣 (xuan)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "nong'tang", text: "弄堂", correct_key: "long'tang", hint: "弄 (long) 堂 (tang)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "nuan'he", text: "暖和", correct_key: "nuan'huo", hint: "暖 (nuan) 和 (huo)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "nuo'da", text: "偌大", correct_key: "ruo'da", hint: "偌 (ruo) 大 (da)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "pan'yang'hu", text: "鄱阳湖", correct_key: "po'yang'hu", hint: "鄱 (po)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "pao'fen", text: "剖分", correct_key: "pou'fen", hint: "剖 (pou) 分 (fen)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "pao'fu'chan", text: "剖腹产", correct_key: "pou'fu'chan", hint: "剖 (pou)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "pao'gong'chan", text: "剖宫产", correct_key: "pou'gong'chan", hint: "剖 (pou)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "pi'sa'bing", text: "比萨饼", correct_key: "bi'sa'bing", hint: "比 (bi)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "pi'yi", text: "裨益", correct_key: "bi'yi", hint: "裨 (bi) 益 (yi)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "po'ji'pao", text: "迫击炮", correct_key: "pai'ji'pao", hint: "迫 (pai)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "pu'an'suan", text: "脯氨酸", correct_key: "fu'an'suan", hint: "脯 (fu)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "pu'gao", text: "讣告", correct_key: "fu'gao", hint: "讣 (fu) 告 (gao)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "qi'ke'fu", text: "契诃夫", correct_key: "qi'he'fu", hint: "诃 (he)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "qian'ma'zhen", text: "荨麻疹", correct_key: "xun'ma'zhen", hint: "荨 (xun)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "qing'ping'le", text: "清平乐", correct_key: "qing'ping'yue", hint: "乐 (yue)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "qing'zha", text: "倾轧", correct_key: "qing'ya", hint: "倾 (qing) 轧 (ya)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "ren'chen", text: "妊娠", correct_key: "ren'shen", hint: "妊 (ren) 娠 (shen)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "ren'mo'gou'yang", text: "人模狗样", correct_key: "ren'mu'gou'yang", hint: "模 (mu)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "rou'pu", text: "肉脯", correct_key: "rou'fu", hint: "肉 (rou) 脯 (fu)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "rou'yi", text: "柔荑", correct_key: "rou'ti", hint: "柔 (rou) 荑 (ti)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "rou'yi'hua'xu", text: "柔荑花序", correct_key: "rou'ti'hua'xu", hint: "荑 (ti)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "san'wei'zhen'huo", text: "三昧真火", correct_key: "san'mei'zhen'huo", hint: "昧 (mei)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "sha'cao", text: "莎草", correct_key: "suo'cao", hint: "莎 (suo) 草 (cao)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "sha'di'li", text: "刹帝利", correct_key: "cha'di'li", hint: "刹 (cha)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "shan'mu", text: "杉木", correct_key: "sha'mu", hint: "杉 (sha) 木 (mu)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "shan'qi'da'deng", text: "氙气大灯", correct_key: "xian'qi'da'deng", hint: "氙 (xian)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "shan'qi'deng", text: "氙气灯", correct_key: "xian'qi'deng", hint: "氙 (xian)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "shan'qi'shou'dian'tong", text: "氙气手电筒", correct_key: "xian'qi'shou'dian'tong", hint: "氙 (xian)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "shang'feng'die", text: "裳凤蝶", correct_key: "chang'feng'die", hint: "裳 (chang)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "shang'jia", text: "商贾", correct_key: "shang'gu", hint: "商 (shang) 贾 (gu)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "shen'tu", text: "神荼", correct_key: "shen'shu", hint: "神 (shen) 荼 (shu)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "shen'tu'yu'lei", text: "神荼郁垒", correct_key: "shen'shu'yu'lv", hint: "荼 (shu) 垒 (lv)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "shou'ru'rou'yi", text: "手如柔荑", correct_key: "shou'ru'rou'ti", hint: "荑 (ti)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "si'ye", text: "肄业", correct_key: "yi'ye", hint: "肄 (yi) 业 (ye)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "suo'sha'mi", text: "缩砂密", correct_key: "su'sha'mi", hint: "缩 (su)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "ta'sha'xing", text: "踏莎行", correct_key: "ta'suo'xing", hint: "莎 (suo)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "tai'xing'shan", text: "太行山", correct_key: "tai'hang'shan", hint: "行 (hang)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "tang'mu'jie'she", text: "瞠目结舌", correct_key: "cheng'mu'jie'she", hint: "瞠 (cheng)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "teng'man", text: "藤蔓", correct_key: "teng'wan", hint: "藤 (teng) 蔓 (wan)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "ti'an", text: "堤岸", correct_key: "di'an", hint: "堤 (di) 岸 (an)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "ti'xian", text: "体癣", correct_key: "ti'xuan", hint: "体 (ti) 癣 (xuan)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "tiao'huan", text: "调换", correct_key: "diao'huan", hint: "调 (diao) 换 (huan)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "tong'ti", text: "胴体", correct_key: "dong'ti", hint: "胴 (dong) 体 (ti)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "tong'xia", text: "恫吓", correct_key: "dong'he", hint: "恫 (dong) 吓 (he)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "tou'pi'xue", text: "头皮屑", correct_key: "tou'pi'xie", hint: "屑 (xie)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "tu'fan", text: "吐蕃", correct_key: "tu'bo", hint: "吐 (tu) 蕃 (bo)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "tu'gu'hun", text: "吐谷浑", correct_key: "tu'yu'hun", hint: "谷 (yu)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "tuo'ben", text: "拓本", correct_key: "ta'ben", hint: "拓 (ta) 本 (ben)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "wen'bo", text: "榅桲", correct_key: "wen'po", hint: "榅 (wen) 桲 (po)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "wen'ting'jun", text: "温庭筠", correct_key: "wen'ting'yun", hint: "筠 (yun)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "xi'yun", text: "吸吮", correct_key: "xi'shun", hint: "吸 (xi) 吮 (shun)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "xia'chi", text: "挟持", correct_key: "xie'chi", hint: "挟 (xie) 持 (chi)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "xian'mei", text: "谄媚", correct_key: "chan'mei", hint: "谄 (chan) 媚 (mei)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "xiang'dao", text: "巷道", correct_key: "hang'dao", hint: "巷 (hang) 道 (dao)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "xiao'dui", text: "校对", correct_key: "jiao'dui", hint: "校 (jiao) 对 (dui)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "xiao'mu'jun", text: "酵母菌", correct_key: "jiao'mu'jun", hint: "酵 (jiao)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "xiao'zheng", text: "校正", correct_key: "jiao'zheng", hint: "校 (jiao) 正 (zheng)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "xie'hu", text: "潟湖", correct_key: "xi'hu", hint: "潟 (xi) 湖 (hu)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "xin'guang'ti'pang", text: "心广体胖", correct_key: "xin'guang'ti'pan", hint: "胖 (pan)", base_cost: 11.60, near_synonym: true },
    MisreadEntry { misread_key: "xin'kuan'ti'pang", text: "心宽体胖", correct_key: "xin'kuan'ti'pan", hint: "胖 (pan)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "xin'xin'xue'zi", text: "莘莘学子", correct_key: "shen'shen'xue'zi", hint: "莘 (shen)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "xing'su", text: "星宿", correct_key: "xing'xiu", hint: "星 (xing) 宿 (xiu)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "xu'yu'wei'she", text: "虚与委蛇", correct_key: "xu'yu'wei'yi", hint: "蛇 (yi)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "xue'fo'lan", text: "雪佛兰", correct_key: "xue'fu'lan", hint: "佛 (fu)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "xun'ma", text: "荨麻", correct_key: "qian'ma", hint: "荨 (qian) 麻 (ma)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "ya'lv'jiang", text: "鸭绿江", correct_key: "ya'lu'jiang", hint: "绿 (lu)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "yi'bao'shi'han", text: "一曝十寒", correct_key: "yi'pu'shi'han", hint: "曝 (pu)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "yi'ji'du'ren", text: "以己度人", correct_key: "yi'ji'duo'ren", hint: "度 (duo)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "yi'mo'yi'yang", text: "一模一样", correct_key: "yi'mu'yi'yang", hint: "模 (mu)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "yi'ye'bian'zhou", text: "一叶扁舟", correct_key: "yi'ye'pian'zhou", hint: "扁 (pian)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "yi'yu'cheng'zhen", text: "一语成谶", correct_key: "yi'yu'cheng'chen", hint: "谶 (chen)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "yin'hong", text: "殷红", correct_key: "yan'hong", hint: "殷 (yan) 红 (hong)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "yin'jiu'jie'ke", text: "饮鸩解渴", correct_key: "yin'zhen'jie'ke", hint: "鸩 (zhen)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "yin'jiu'zhi'ke", text: "饮鸩止渴", correct_key: "yin'zhen'zhi'ke", hint: "鸩 (zhen)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "yin'li", text: "阴霾", correct_key: "yin'mai", hint: "阴 (yin) 霾 (mai)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "ying'pin", text: "赝品", correct_key: "yan'pin", hint: "赝 (yan) 品 (pin)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "you'mo'you'yang", text: "有模有样", correct_key: "you'mu'you'yang", hint: "模 (mu)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "you'shuo", text: "游说", correct_key: "you'shui", hint: "游 (you) 说 (shui)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "yu'he", text: "潏河", correct_key: "jue'he", hint: "潏 (jue) 河 (he)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "yu'lei", text: "郁垒", correct_key: "yu'lv", hint: "郁 (yu) 垒 (lv)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "yu'lei'shen'tu", text: "郁垒神荼", correct_key: "yu'lv'shen'shu", hint: "垒 (lv) 荼 (shu)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "yuan'sui", text: "芫荽", correct_key: "yan'sui", hint: "芫 (yan) 荽 (sui)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "zhang'bai'zhi", text: "张柏芝", correct_key: "zhang'bo'zhi", hint: "柏 (bo)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "zhao'xiang", text: "着想", correct_key: "zhuo'xiang", hint: "着 (zhuo) 想 (xiang)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "zheng'shang", text: "整饬", correct_key: "zheng'chi", hint: "整 (zheng) 饬 (chi)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "zhi'ku", text: "桎梏", correct_key: "zhi'gu", hint: "桎 (zhi) 梏 (gu)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "zhi're", text: "炽热", correct_key: "chi're", hint: "炽 (chi) 热 (re)", base_cost: 11.60, near_synonym: true },
    MisreadEntry { misread_key: "zhi'sheng", text: "吱声", correct_key: "zi'sheng", hint: "吱 (zi) 声 (sheng)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "zhi'zhou", text: "掣肘", correct_key: "che'zhou", hint: "掣 (che) 肘 (zhou)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "zhong'le'le", text: "众乐乐", correct_key: "zhong'yue'le", hint: "乐 (yue)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "zhu'you'tang", text: "朱祐樘", correct_key: "zhu'you'cheng", hint: "樘 (cheng)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "zhuang'mo'zuo'yang", text: "装模作样", correct_key: "zhuang'mu'zuo'yang", hint: "模 (mu)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "zhui'xin'qi'xie", text: "椎心泣血", correct_key: "chui'xin'qi'xue", hint: "椎 (chui) 血 (xue)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "zhui'xin'qi'xue", text: "椎心泣血", correct_key: "chui'xin'qi'xue", hint: "椎 (chui)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "zhuo'zhuo'bi'ren", text: "咄咄逼人", correct_key: "duo'duo'bi'ren", hint: "咄 (duo)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "zi'yuan'zi'ai", text: "自怨自艾", correct_key: "zi'yuan'zi'yi", hint: "艾 (yi)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "zong'zong", text: "淙淙", correct_key: "cong'cong", hint: "淙 (cong) 淙 (cong)", base_cost: 11.60, near_synonym: false },
    MisreadEntry { misread_key: "zu'xian", text: "足癣", correct_key: "zu'xuan", hint: "足 (zu) 癣 (xuan)", base_cost: 11.60, near_synonym: false },
];

struct MisreadIndex {
    trie: FlatTrie,
    flat_map: HashMap<(String, &'static str), &'static str>,
}

static MISREAD_INDEX: LazyLock<MisreadIndex> = LazyLock::new(|| {
    let mut trie = FlatTrie::new();
    let mut flat_map = HashMap::with_capacity(MISREAD_ENTRIES.len());
    for (idx, entry) in MISREAD_ENTRIES.iter().enumerate() {
        trie.insert(entry.misread_key, idx as u32, false, 0);
        let flat: String = entry.misread_key.chars().filter(|&c| c != '\'').collect();
        flat_map.entry((flat, entry.text)).or_insert(entry.hint);
    }
    MisreadIndex { trie, flat_map }
});

/// 判断 `(无分隔或带分隔的输入串, 候选词文本)` 是否命中常见误读表，若命中则返回正确读音标注。
/// 用于个人词库记忆（`[记忆·N次]`）召回时保留读音提示，同时确保按正音输入时不误标。
pub fn lookup_hint_flat(raw_input: &str, text: &str) -> Option<&'static str> {
    let flat: String = raw_input
        .chars()
        .filter(|&c| c != '\'' && !c.is_whitespace())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    MISREAD_INDEX.flat_map.get(&(flat, text)).copied()
}

/// 判断词条在给定误读键下是否属于近义词混淆（如 `zhire` 下「炙热」与「炽热」），供 AI 候选池过滤。
pub fn is_near_synonym_misread(misread_key: &str, text: &str) -> bool {
    for &id in MISREAD_INDEX.trie.exact_lookup(misread_key) {
        let e = &MISREAD_ENTRIES[id as usize];
        if e.text == text && e.near_synonym {
            return true;
        }
    }
    false
}

/// 从 `origin_start` 出发沿精确全拼音节步（`fuzzy_penalty == 0.0`）检索常见易错音词条并注入晶格图。
pub fn collect_misread_edges(
    origin_start: usize,
    input: &str,
    spans_at: &[Vec<&SyllableSpan>],
    out_edges: &mut Vec<LatticeEdge>,
) {
    collect_dfs(origin_start, origin_start, 0, 0, input, spans_at, out_edges);
}

fn collect_dfs(
    origin_start: usize,
    pos: usize,
    curr_node: u32,
    depth: usize,
    input: &str,
    spans_at: &[Vec<&SyllableSpan>],
    out_edges: &mut Vec<LatticeEdge>,
) {
    if depth >= 6 || pos >= input.len() {
        return;
    }
    let mut effective_pos = pos;
    if depth > 0 {
        while effective_pos < input.len() && input.as_bytes()[effective_pos] == b'\'' {
            effective_pos += 1;
        }
        if effective_pos >= input.len() {
            return;
        }
    }
    let trie = &MISREAD_INDEX.trie;
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
        if sp.fuzzy_penalty > 0.0 {
            continue;
        }
        let Some(next_node) = trie.advance_node(base_node, &sp.syllable) else {
            continue;
        };
        let hit_ids = &trie.nodes[next_node as usize].exact_entries;
        if !hit_ids.is_empty() {
            let raw_span = &input[origin_start..sp.end];
            let splits_single_syl = syl_count >= 2 && is_unapostrophed_single_syllable(raw_span);
            if !splits_single_syl {
                let best_exact_cost = out_edges
                    .iter()
                    .filter(|e| {
                        e.start == origin_start
                            && e.end == sp.end
                            && !e.is_fuzzy
                            && e.pronunciation_hint.is_none()
                            && e.syllable_count == syl_count
                    })
                    .map(|e| e.base_cost)
                    .fold(None::<f32>, |acc, c| Some(acc.map_or(c, |a| a.min(c))));

                for (rank, &id) in hit_ids.iter().enumerate() {
                    let entry = &MISREAD_ENTRIES[id as usize];
                    // 若词库本身或模糊音已生成同区间同文本边，直接升级附着正音提示，不重复压入
                    if let Some(existing) = out_edges.iter_mut().find(|e| {
                        e.start == origin_start && e.end == sp.end && e.text == entry.text
                    }) {
                        existing.pronunciation_hint = Some(entry.hint);
                        if existing.is_fuzzy {
                            existing.is_fuzzy = false;
                            existing.code_key = entry.misread_key.to_string();
                        }
                        continue;
                    }
                    // 若存在常用正音同音词（如 `hundun` 的「混沌」`cost <= 13.5`），保持正音词排第 1，
                    // 易错音词紧随其后；若仅有生僻词或无同音词（如 `jiaose`、`geiyu`、`paoxi`），直接按自身词频排前。
                    let cost = match best_exact_cost {
                        Some(best) if best <= 13.5 => {
                            entry.base_cost.max(best + 0.55 + rank as f32 * 0.08)
                        }
                        _ => entry.base_cost + rank as f32 * 0.08,
                    };
                    out_edges.push(LatticeEdge {
                        start: origin_start,
                        end: sp.end,
                        text: entry.text.to_string(),
                        code_key: entry.misread_key.to_string(),
                        kind: EdgeKind::ChineseWord,
                        base_cost: cost,
                        domain: DomainTag::General,
                        syllable_count: syl_count,
                        is_fuzzy: false,
                        pronunciation_hint: Some(entry.hint),
                    });
                }
            }
        }
        collect_dfs(
            origin_start,
            sp.end,
            next_node,
            syl_count,
            input,
            spans_at,
            out_edges,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn misread_table_has_clean_bmp_entries_and_expected_hints() {
        assert!(MISREAD_ENTRIES.len() >= 240);
        for e in MISREAD_ENTRIES {
            assert!(!e.text.is_empty());
            assert!(e.text.chars().all(|c| (c as u32) <= 0xFFFF), "non-BMP char in {}", e.text);
            assert_ne!(e.misread_key, e.correct_key, "identical key for {}", e.text);
            assert!(!e.hint.is_empty());
        }
        assert_eq!(lookup_hint_flat("hundun", "馄饨"), Some("馄 (hun) 饨 (tun)"));
        assert_eq!(lookup_hint_flat("hun'dun", "馄饨"), Some("馄 (hun) 饨 (tun)"));
        assert_eq!(lookup_hint_flat("huntun", "馄饨"), None);
        assert_eq!(lookup_hint_flat("jiaose", "角色"), Some("角 (jue) 色 (se)"));
        assert_eq!(lookup_hint_flat("youhuijuan", "优惠券"), Some("券 (quan)"));
    }
}
