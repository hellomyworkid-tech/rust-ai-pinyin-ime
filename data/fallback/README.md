# 基础词库的数据（iOS 键盘）

`aosp_pinyin.dict.yaml` 由 AOSP 拼音输入法的词库转换而来，供 iOS 键盘的「基础词库」用：完整词库（雾凇拼音，
GPL-3.0-only）不打进 App Store 的安装包，由 App 第一次打开时下载；下完之前键盘用基础词库打中文。

- 来源：https://android.googlesource.com/platform/packages/inputmethods/PinyinIME 的
  `jni/data/rawdict_utf16_65105_freq.txt`（提交 49aebad1c1cfbbcaa9288ffed5161e79e57c3679，2014-05-14；
  文件 3,570,346 字节，SHA-256 408700f28a56091fa07f3b849a0f134fbfc71e6b2ae9b3f52973a5b076f599ff）
- 转换：`python3 -I tools/lexicon/aosp_to_rime.py <rawdict_utf16_65105_freq.txt> data/fallback/aosp_pinyin.dict.yaml`
  （65,105 条，权重 = 原频率 × 100）
- 编译：`platforms/ios/scripts/build-fallback-lexicon.sh` → `platforms/ios/Resources/fallback_lexicon.bin`
- 协议：Apache License 2.0，Copyright (C) 2009 The Android Open Source Project。全文见同目录的 `NOTICE`（原样拷自 AOSP）
