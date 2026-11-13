#!/bin/zsh
# 在模拟器里启用「AI 拼音」键盘并设为当前键盘，不用手点 设置 → 通用 → 键盘。
#
# 用法：scripts/enable-keyboard.sh [模拟器 UDID 或名称]（默认 iPhone 18 Pro）
#
# 做的事：
# 1. 写模拟器的全局偏好 AppleKeyboards：已启用的键盘列表，第三方键盘用扩展的 bundle id 表示；
# 2. 写 com.apple.keyboard.preferences 的 KeyboardsCurrentAndNext：当前键盘设为 AI 拼音；
# 3. 关掉「连着硬件键盘时把软键盘收成小条」（AutomaticMinimizationEnabled），
#    否则 UI 测试里键盘画在屏幕外面；
# 4. 重启模拟器让键盘服务重新读取。
# App 要先装好（xcrun simctl install），键盘扩展随 App 一起装。
set -euo pipefail

DEVICE="${1:-iPhone 18 Pro}"
KEYBOARD_ID="com.zengji.aipinyin.keyboard"

xcrun simctl boot "$DEVICE" 2>/dev/null || true
xcrun simctl bootstatus "$DEVICE" -b >/dev/null

xcrun simctl spawn "$DEVICE" defaults write -g AppleKeyboards -array \
    "$KEYBOARD_ID" \
    "zh_Hans-Pinyin@sw=Pinyin-Simplified;hw=Automatic" \
    "emoji@sw=Emoji" \
    "en_US@sw=QWERTY;hw=Automatic"
xcrun simctl spawn "$DEVICE" defaults write -g AppleKeyboardsExpanded -int 1
xcrun simctl spawn "$DEVICE" defaults write com.apple.keyboard.preferences KeyboardsCurrentAndNext -array \
    "$KEYBOARD_ID" \
    "zh_Hans-Pinyin@sw=Pinyin-Simplified;hw=Automatic"
xcrun simctl spawn "$DEVICE" defaults write com.apple.keyboard.preferences AutomaticMinimizationEnabled -bool false
xcrun simctl spawn "$DEVICE" defaults write com.apple.keyboard.preferences HardwareKeyboardLastSeen -bool false

xcrun simctl shutdown "$DEVICE"
xcrun simctl boot "$DEVICE"
xcrun simctl bootstatus "$DEVICE" -b >/dev/null

echo "已启用：$(xcrun simctl spawn "$DEVICE" defaults read -g AppleKeyboards | tr -d '\n ')"
