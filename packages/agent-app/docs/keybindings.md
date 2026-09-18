# 快捷键

所有键盘快捷键都可以通过 `~/.agent-core/agent/keybindings.json` 自定义。每个动作可以绑定一个或多个按键。

配置文件使用与 pi 内部一致、扩展作者在 `keyHint()` 和注入的 `keybindings` 管理器中也使用的带命名空间快捷键 id。

使用旧版无命名空间 id（如 `cursorUp` 或 `expandTools`）的旧配置会在启动时自动迁移为带命名空间的 id。

编辑 `keybindings.json` 后，在 agent-core 中运行 `/reload` 即可应用更改，无需重启会话。

## 按键格式

`modifier+key` 格式，修饰键为 `ctrl`、`shift`、`alt`、`super`（可组合），按键包括：

- **字母：** `a-z`
- **数字：** `0-9`
- **特殊键：** `escape`, `esc`, `enter`, `return`, `tab`, `space`, `backspace`, `delete`, `insert`, `clear`, `home`, `end`, `pageUp`, `pageDown`, `up`, `down`, `left`, `right`
- **功能键：** `f1`-`f12`
- **符号：** `` ` ``, `-`, `=`, `[`, `]`, `\`, `;`, `'`, `,`, `.`, `/`, `!`, `@`, `#`, `$`, `%`, `^`, `&`, `*`, `(`, `)`, `_`, `+`, `|`, `~`, `{`, `}`, `:`, `<`, `>`, `?`

修饰键组合：`ctrl+shift+x`、`alt+ctrl+x`、`ctrl+shift+alt+x`、`super+k`、`ctrl+super+k`、`ctrl+1` 等。

`super` 绑定需要终端单独上报该修饰键，通常通过 Kitty 键盘协议实现。在不支持该协议的终端中可能无法使用。

## 全部动作

### TUI 编辑器光标移动

| 快捷键 id | 默认值 | 说明 |
|--------|---------|-------------|
| `tui.editor.cursorUp` | `up` | 上移光标，顶部可浏览更早的历史 |
| `tui.editor.cursorDown` | `down` | 下移光标，底部可浏览更新的历史 |
| `tui.editor.historyPrevious` | *（无）* | 选择上一条提示历史 |
| `tui.editor.historyNext` | *（无）* | 选择下一条提示历史 |
| `tui.editor.cursorLeft` | `left`, `ctrl+b` | 左移光标 |
| `tui.editor.cursorRight` | `right`, `ctrl+f` | 右移光标 |
| `tui.editor.cursorWordLeft` | `alt+left`, `ctrl+left`, `alt+b` | 光标左移一个词 |
| `tui.editor.cursorWordRight` | `alt+right`, `ctrl+right`, `alt+f` | 光标右移一个词 |
| `tui.editor.cursorLineStart` | `home`, `ctrl+home`, `ctrl+a` | 移到行首 |
| `tui.editor.cursorLineEnd` | `end`, `ctrl+end`, `ctrl+e` | 移到行尾 |
| `tui.editor.jumpForward` | `ctrl+]` | 向前跳到指定字符 |
| `tui.editor.jumpBackward` | `ctrl+alt+]` | 向后跳到指定字符 |
| `tui.editor.pageUp` | `pageUp`, `ctrl+pageUp` | 向上滚动一页 |
| `tui.editor.pageDown` | `pageDown`, `ctrl+pageDown` | 向下滚动一页 |

专用的历史动作总是操作历史条目，与光标在多行提示中的位置无关。主编辑器聚焦时，显式的历史绑定优先于应用动作，因此把 `tui.editor.historyPrevious` 绑定到 `ctrl+p` 会在该上下文中覆盖路径显示切换，而不影响选择器中的 `Ctrl+P`。

### TUI 编辑器删除

| 快捷键 id | 默认值 | 说明 |
|--------|---------|-------------|
| `tui.editor.deleteCharBackward` | `backspace` | 向后删除一个字符 |
| `tui.editor.deleteCharForward` | `delete`, `ctrl+d` | 向前删除一个字符 |
| `tui.editor.deleteWordBackward` | `ctrl+w`, `alt+backspace` | 向后删除一个词 |
| `tui.editor.deleteWordForward` | `alt+d`, `alt+delete` | 向前删除一个词 |
| `tui.editor.deleteToLineStart` | `ctrl+u` | 删除到行首 |
| `tui.editor.deleteToLineEnd` | `ctrl+k` | 删除到行尾 |

### TUI 输入

| 快捷键 id | 默认值 | 说明 |
|--------|---------|-------------|
| `tui.input.newLine` | `shift+enter`, `ctrl+j` | 插入新行 |
| `tui.input.submit` | `enter` | 提交输入 |
| `tui.input.tab` | `tab` | Tab / 自动补全 |

### TUI 剪贴环（Kill Ring）

| 快捷键 id | 默认值 | 说明 |
|--------|---------|-------------|
| `tui.editor.yank` | `ctrl+y` | 粘贴最近删除的文本 |
| `tui.editor.yankPop` | `alt+y` | yank 后在更早删除的文本间轮换 |
| `tui.editor.undo` | `ctrl+-`（Windows 上为 `ctrl+z`；WSL 上为 `alt+z`） | 撤销上次编辑 |

### TUI 剪贴板与选择

| 快捷键 id | 默认值 | 说明 |
|--------|---------|-------------|
| `tui.input.copy` | `ctrl+c` | 复制选中内容 |
| `tui.select.up` | `up` | 上移选择 |
| `tui.select.down` | `down` | 下移选择 |
| `tui.select.pageUp` | `pageUp` | 列表中向上翻页 |
| `tui.select.pageDown` | `pageDown` | 列表中向下翻页 |
| `tui.select.confirm` | `enter` | 确认选择 |
| `tui.select.cancel` | `escape`, `ctrl+c` | 取消选择 |

### TUI 全屏视口

这些动作在交互模式使用 `--tui-mode fullscreen` 时生效，作用于主对话记录滚动区域。双指触控板和鼠标滚轮滚动指针下方区域，在固定的编辑器/状态/页脚停靠区上方则回退到对话记录。点击 OSC 8 超链接会用默认处理器打开。按住主鼠标键拖动可选中文字并复制到剪贴板；在对话记录顶部或底部边缘按住不放会自动滚动到屏外内容。对话记录向上滚动时，其底行会显示可点击的“跳到最新消息”标签，并标注 `tui.altScreen.bottom` 快捷键。终端相关的鼠标和触控板行为见 [终端配置](terminal-setup.md)。

全屏对话记录绑定优先于编辑器绑定。因此在全屏模式下，默认不带修饰键的导航键控制对话记录，其 `ctrl` 变体仍控制编辑器。非全屏模式下，两种变体都控制编辑器。

对话记录搜索面板会显示已配置的上一个/下一个快捷键和可点击的箭头控件。再次按 `tui.altScreen.search`，或使用 `tui.altScreen.searchClose`，即可关闭。

| 按键 | 默认模式 | 全屏模式 |
|-----|--------------|-----------------|
| `home`, `end` | 编辑器 | 对话记录 |
| `ctrl+home`, `ctrl+end` | 编辑器 | 编辑器 |
| `pageUp`, `pageDown` | 编辑器 | 对话记录 |
| `ctrl+pageUp`, `ctrl+pageDown` | 编辑器 | 编辑器 |

这一分配仍可通过普通动作绑定配置。例如 `"tui.altScreen.pageUp": "ctrl+pageUp"` 会让 `pageUp` 控制编辑器、`ctrl+pageUp` 在全屏模式下控制对话记录。绑定 `tui.altScreen.halfPageUp` 和 `tui.altScreen.halfPageDown` 实现半页滚动，或绑定 `tui.altScreen.lineUp` 和 `tui.altScreen.lineDown` 实现单行滚动。设置 `"tui.altScreen.pageUp": []` 会完全禁用该对话记录快捷键。用户绑定会替换该动作的默认值。

| 快捷键 id | 默认值 | 说明 |
|--------|---------|-------------|
| `tui.altScreen.pageUp` | `pageUp` | 对话记录向上滚动一页 |
| `tui.altScreen.pageDown` | `pageDown` | 对话记录向下滚动一页 |
| `tui.altScreen.halfPageUp` | *（无）* | 对话记录向上滚动半页 |
| `tui.altScreen.halfPageDown` | *（无）* | 对话记录向下滚动半页 |
| `tui.altScreen.lineUp` | *（无）* | 对话记录向上滚动一行 |
| `tui.altScreen.lineDown` | *（无）* | 对话记录向下滚动一行 |
| `tui.altScreen.previousPrompt` | `ctrl+shift+up`, `ctrl+up`（仅 Windows 和 WSL 上为 `ctrl+up`） | 跳到上一个标记的消息 |
| `tui.altScreen.nextPrompt` | `ctrl+shift+down`, `ctrl+down`（仅 Windows 和 WSL 上为 `ctrl+down`） | 跳到下一个标记的消息 |
| `tui.altScreen.search` | `ctrl+shift+f`（Windows 和 WSL 上为 `ctrl+f`） | 搜索已渲染的对话记录 |
| `tui.altScreen.searchNext` | `enter`, `ctrl+g` | 搜索时选中下一个匹配 |
| `tui.altScreen.searchPrevious` | `shift+enter`, `ctrl+shift+g` | 搜索时选中上一个匹配 |
| `tui.altScreen.searchClose` | `escape` | 关闭对话记录搜索 |
| `tui.altScreen.top` | `home` | 滚动到对话记录开头 |
| `tui.altScreen.bottom` | `end` | 滚动到对话记录末尾并跟随新输出 |

### 应用

| 快捷键 id | 默认值 | 说明 |
|--------|---------|-------------|
| `app.interrupt` | `escape` | 取消或中止 |
| `app.clear` | `ctrl+c` | 清空编辑器（第一次）/ 退出（第二次） |
| `app.exit` | `ctrl+d` | 退出（编辑器为空时） |
| `app.suspend` | `ctrl+z`（Windows 上无） | 挂起到后台 |
| `app.editor.external` | `ctrl+g` | 在外部编辑器中打开（`externalEditor`、`$VISUAL`、`$EDITOR`、Windows 上的记事本，或其他平台上的 `nano`） |
| `app.clipboard.pasteImage` | `ctrl+v`（Windows 和 WSL 上为 `alt+v`） | 从剪贴板粘贴图像或文本 |

### 会话

| 快捷键 id | 默认值 | 说明 |
|--------|---------|-------------|
| `app.session.new` | *（无）* | 开始新会话（`/new`） |
| `app.session.tree` | *（无）* | 打开会话树导航器（`/tree`） |
| `app.session.fork` | *（无）* | 分叉当前会话（`/fork`） |
| `app.session.resume` | *（无）* | 打开会话恢复选择器（`/resume`） |
| `app.session.togglePath` | `ctrl+p` | 切换路径显示 |
| `app.session.toggleSort` | `ctrl+s` | 切换排序方式 |
| `app.session.toggleNamedFilter` | `ctrl+n` | 切换“仅已命名会话”筛选 |
| `app.session.rename` | `ctrl+r` | 重命名会话 |
| `app.session.delete` | `ctrl+d` | 删除会话 |
| `app.session.deleteNoninvasive` | `ctrl+backspace` | 查询为空时删除会话 |

### 模型与思考

| 快捷键 id | 默认值 | 说明 |
|--------|---------|-------------|
| `app.model.select` | `ctrl+l` | 打开模型选择器 |
| `app.models.save` | `ctrl+s` | 将选中的默认模型保存到设置 |
| `app.thinking.cycle` | `shift+tab` | 循环切换思考级别 |
| `app.thinking.save` | `ctrl+s` | 将当前思考级别保存到设置 |
| `app.thinking.toggle` | `ctrl+t` | 折叠或展开思考块 |

### 显示与消息队列

| 快捷键 id | 默认值 | 说明 |
|--------|---------|-------------|
| `app.tools.expand` | `ctrl+o` | 折叠或展开工具输出 |
| `app.message.followUp` | `alt+enter`（Windows 和 WSL 上为 `ctrl+q`） | 入队追加消息 |
| `app.message.dequeue` | `alt+up`（Windows 和 WSL 上为 `alt+q`） | 将队列中的消息恢复到编辑器 |

### 树导航

| 快捷键 id | 默认值 | 说明 |
|--------|---------|-------------|
| `app.tree.foldOrUp` | `ctrl+left`, `alt+left` | 折叠当前分支段，或跳到上一段起点 |
| `app.tree.unfoldOrDown` | `ctrl+right`, `alt+right` | 展开当前分支段，或跳到下一段起点或分支末尾 |
| `app.tree.editLabel` | `shift+l` | 编辑所选树节点的标签 |
| `app.tree.toggleLabelTimestamp` | `shift+t` | 切换树中标签时间戳的显示 |
| `app.tree.filter.default` | `ctrl+d` | 将树筛选设为默认视图 |
| `app.tree.filter.noTools` | `ctrl+t` | 切换隐藏工具结果的树筛选 |
| `app.tree.filter.userOnly` | `ctrl+u` | 切换只显示用户消息的树筛选 |
| `app.tree.filter.labeledOnly` | `ctrl+l` | 切换只显示带标签条目的树筛选 |
| `app.tree.filter.all` | `ctrl+a` | 切换显示全部条目的树筛选 |
| `app.tree.filter.cycleForward` | `ctrl+o` | 向前循环切换树筛选 |
| `app.tree.filter.cycleBackward` | `shift+ctrl+o` | 向后循环切换树筛选 |

## 自定义配置

创建 `~/.agent-core/agent/keybindings.json`：

```json
{
  "tui.editor.historyPrevious": "ctrl+p",
  "tui.editor.historyNext": "ctrl+n",
  "tui.editor.deleteWordBackward": ["ctrl+w", "alt+backspace"]
}
```

每个动作可以是单个按键或按键数组。用户配置覆盖默认值。

在原生 Windows 上，`app.suspend` 没有默认绑定，因为 Windows 终端不支持 Unix 作业控制。手动绑定后，pi 会显示状态消息而不是挂起。在 WSL 中，正常的 Linux `ctrl+z`/`fg` 行为仍然适用。

### Emacs 示例

```json
{
  "tui.editor.historyPrevious": "ctrl+p",
  "tui.editor.historyNext": "ctrl+n",
  "tui.editor.cursorLeft": ["left", "ctrl+b"],
  "tui.editor.cursorRight": ["right", "ctrl+f"],
  "tui.editor.cursorWordLeft": ["alt+left", "alt+b"],
  "tui.editor.cursorWordRight": ["alt+right", "alt+f"],
  "tui.editor.deleteCharForward": ["delete", "ctrl+d"],
  "tui.editor.deleteCharBackward": ["backspace", "ctrl+h"],
  "tui.input.newLine": ["shift+enter", "ctrl+j"]
}
```

### Vim 示例

```json
{
  "tui.editor.cursorUp": ["up", "alt+k"],
  "tui.editor.cursorDown": ["down", "alt+j"],
  "tui.editor.cursorLeft": ["left", "alt+h"],
  "tui.editor.cursorRight": ["right", "alt+l"],
  "tui.editor.cursorWordLeft": ["alt+left", "alt+b"],
  "tui.editor.cursorWordRight": ["alt+right", "alt+w"]
}
```
