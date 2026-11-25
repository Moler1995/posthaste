//! The English and Chinese catalogue.
//!
//! Every user-visible interface string is declared here exactly once, with its
//! text in both languages. `text_table!` entries are fixed labels;
//! `message_table!` entries are whole sentences with `{}` placeholders that
//! [`super::msg`] fills. A key with no text in one language does not compile.
//!
//! Declaration order is the reading order of the interface, so a translation
//! review can walk the catalogue top to bottom.

text_table! {
    // The settings dialog.
    SettingsTitle => "Settings" / "设置",
    SettingsSubtitle => "Changes apply immediately and save automatically." / "修改会立即生效并自动保存。",
    SettingsStoredOutside => "Preferences are stored outside the workspace." / "偏好设置保存在工作区之外。",

    SectionAppearance => "Appearance" / "外观",
    SectionLanguage => "Language" / "语言",
    SectionRequests => "Requests" / "请求",
    SectionEditing => "Editing" / "编辑",
    SectionHistory => "History" / "历史",

    SettingTheme => "Theme" / "主题",
    SettingThemeHint => "Dark, Light, or the operating system's appearance." / "深色、浅色，或跟随操作系统的外观。",
    ThemeDark => "Dark" / "深色",
    ThemeLight => "Light" / "浅色",
    ThemeSystem => "System" / "跟随系统",

    SettingWrapLongLines => "Wrap long lines" / "自动换行",
    SettingWrapLongLinesHint => "Wrap Pretty and Raw response bodies to the pane width." / "按窗格宽度为美化与原始响应正文换行。",

    SettingLanguage => "Interface language" / "界面语言",
    SettingLanguageHint => "Applies to the interface only; stored data keeps its names." / "仅影响界面，已保存的数据保持原名。",
    // The two option labels are written the same way in both languages.
    LanguageEnglish => "English" / "English",
    LanguageChinese => "简体中文" / "简体中文",

    CommonOff => "Off" / "关闭",
    CommonOn => "On" / "开启",

    SettingDefaultTimeout => "Default timeout" / "默认超时",
    SettingDefaultTimeoutHint => "Applies when a request leaves its timeout empty." / "当请求未填写超时时使用。",
    SettingRedirectLimit => "Redirect limit" / "重定向上限",
    SettingRedirectLimitHint => "Zero disables redirect following." / "零表示不跟随重定向。",
    SettingMaximumBody => "Maximum response body" / "响应正文上限",
    SettingMaximumBodyHint => "Larger bodies are stored truncated." / "更大的正文会被截断保存。",

    SettingAutosave => "Autosave" / "自动保存",
    SettingAutosaveHint => "Write request edits after the debounce. Off keeps them in the tab draft." / "在停顿后写入请求修改。关闭时仅保存在标签草稿中。",
    SettingEditorFontSize => "Editor font size" / "编辑器字号",
    SettingEditorFontSizeHint => "Applies to URLs, headers, bodies, and JSON text only." / "仅用于地址、请求头、正文和 JSON 文本。",

    SettingRetainedEntries => "Retained entries" / "保留条数",
    SettingRetainedEntriesHint => "Lowering the count trims the oldest entries and their files." / "调低数量会删除最旧的记录及其文件。",

    // Shared controls and actions.
    CommonClose => "Close" / "关闭",
    CommonCancel => "Cancel" / "取消",
    CommonSave => "Save" / "保存",
    CommonDelete => "Delete" / "删除",
    CommonCopyValue => "Copy value" / "复制值",
    ActionNewRequest => "New request" / "新建请求",
    ActionSend => "Send" / "发送",
    ActionConnect => "Connect" / "连接",
    ActionDisconnect => "Disconnect" / "断开",

    // The status bar.
    StatusUnsaved => "Unsaved changes" / "未保存的更改",
    StatusSaved => "Saved" / "已保存",

    // Placeholders.
    PlaceholderSearch => "Search…" / "搜索…",
    PlaceholderWorkspaceName => "Workspace name" / "工作区名称",
    PlaceholderCollectionName => "Collection name" / "集合名称",
    PlaceholderRequestName => "Request name" / "请求名称",
    PlaceholderName => "Name" / "名称",
    PlaceholderCurlRequestName => "Named after the command" / "以命令命名",
    PlaceholderCurlCommands => "Paste one or more cURL commands" / "粘贴一个或多个 cURL 命令",
    PlaceholderValue => "Value" / "值",

    // Platform file dialogs.
    PromptOpenWorkspace => "Open a workspace folder" / "打开工作区文件夹",
    PromptCreateWorkspaceAt => "Choose where to create the workspace" / "选择工作区的创建位置",
    PromptImportCollection => "Import a collection" / "导入集合",
    PromptChooseUpload => "Choose a file to upload" / "选择要上传的文件",

    // Plain notices and validation.
    SavePickerChooseDestination => "Choose where to save this request." / "请选择此请求的保存位置。",
    SavePickerEnterName => "Enter a request name." / "请输入请求名称。",
    RenameNameRequired => "Name is required." / "请输入名称。",
    RenameCollectionTaken => "A collection with this name already exists." / "已存在同名集合。",
    RenameFolderTaken => "A folder with this name already exists." / "已存在同名文件夹。",
    RenameEnvironmentTaken => "An environment with this name already exists." / "已存在同名环境。",
    WorkspaceNameRequired => "Enter a workspace name." / "请输入工作区名称。",
    WorkspaceNameInvalidCharacter => "The name contains a character that cannot be used in a folder name." / "名称中包含不能用于文件夹名的字符。",
    WorkspaceNameReserved => "That name is reserved on this platform." / "该名称在此平台上为保留名称。",

    // Save-status causes. The status bar and the tab tooltip wrap these in a
    // sentence, so each reads as its own clause.
    WorkspaceUnavailable => "the workspace is unavailable" / "工作区不可用",
    SaveFolderMissing => "the destination folder no longer exists" / "目标文件夹已不存在",
    SaveRequestsCopyFailed => "some requests could not be copied" / "部分请求无法复制",
    SaveRequestsDeleteFailed => "some requests could not be deleted" / "部分请求无法删除",
    CurlImportRequestNameItem => "request name" / "请求名称",
    CurlImportRequestNameReason => "applies to a single command; these requests kept their inferred names" / "仅适用于单条命令；这些请求保留了推断的名称",
    DestinationCollectionGone => "the destination collection is no longer available" / "目标集合已不再可用",
    ImportNotRecognized => "the file is not a recognizable collection document; expected a .posthaste.json bundle or a Postman collection" / "无法识别该集合文档；应为 .posthaste.json 打包文件或 Postman 集合",

    // Export warnings.
    ExportBundleWarning => "The bundle is plain text and is not encrypted. Headers, auth values, and anything else you saved in this collection are written as-is, so anyone who can read the file can read them." / "打包文件是纯文本，未加密。请求头、认证值以及你在该集合中保存的其他内容都会原样写入，任何能读取此文件的人都能读到它们。",
    ExportPostmanWarning => "The collection file is plain text and is not encrypted. Headers, auth values, and anything else you saved in this collection are written as-is, so anyone who can read the file can read them." / "集合文件是纯文本，未加密。请求头、认证值以及你在该集合中保存的其他内容都会原样写入，任何能读取此文件的人都能读到它们。",

    // The sidebar.
    SidebarCollections => "Collections" / "集合",
    SidebarVariables => "Variables" / "变量",
    SidebarHistory => "History" / "历史",
    SidebarDefaults => "Defaults" / "默认值",
    SidebarNoEnvironment => "No environment" / "无环境",
    SidebarDismiss => "Dismiss" / "忽略",
    SidebarDeleteFile => "Delete file" / "删除文件",
    // Shared empty states.
    CommonNoMatches => "No matches" / "无匹配项",
    SidebarNoHistory => "No history yet" / "尚无历史记录",
    SidebarNoCollections => "No collections yet" / "尚无集合",
    SidebarNoRequests => "No requests" / "尚无请求",
    SidebarImport => "Import" / "导入",
    SidebarImportEllipsis => "Import…" / "导入…",
    SidebarImportFromFile => "Import from file…" / "从文件导入…",
    SidebarFromCurlText => "From cURL text…" / "从 cURL 文本导入…",
    SidebarFromCurl => "From cURL" / "从 cURL",
    SidebarNewCollection => "New collection" / "新建集合",
    SidebarNewFolder => "New folder" / "新建文件夹",
    SidebarNewEnvironment => "+ New environment" / "+ 新建环境",
    SidebarAddVariable => "+ Add variable" / "+ 添加变量",
    SidebarCommands => "Commands" / "命令",
    SidebarEscCloses => "Esc closes" / "Esc 关闭",
    SidebarChooseFile => "Choose file…" / "选择文件…",
    SidebarConfirm => "Confirm" / "确认",
    SidebarClear => "Clear" / "清除",
    SidebarRename => "Rename" / "重命名",
    SidebarDuplicate => "Duplicate" / "复制",
    SidebarMoveTo => "Move to" / "移动到",
    SidebarMoveToEllipsis => "Move to…" / "移动到…",
    SidebarCurrent => "Current" / "当前",
    SidebarExportPosthaste => "Export Posthaste…" / "导出 Posthaste…",
    SidebarExportPostman => "Export Postman…" / "导出 Postman…",
    SidebarRenameHint => "Enter saves · Esc cancels" / "回车保存 · Esc 取消",
    SidebarRenameHintClickAway => "Clicking away also saves" / "点击别处也会保存",
    HistoryFailed => "Failed" / "失败",
    HistoryCancelled => "Cancelled" / "已取消",
    HistoryClosed => "Closed" / "已关闭",

    // Variables.
    PlaceholderVariableName => "name" / "名称",
    PlaceholderVariableValue => "value" / "值",
    EnvironmentNameHeader => "NAME" / "名称",
    EnvironmentValueHeader => "VALUE" / "值",
    EnvironmentNoVariables => "No variables yet" / "尚无变量",
    EnvironmentOverridesGlobal => "Overrides the global value" / "覆盖全局值",
    AssistClickToSet => "Click to set a value." / "点击以设置值。",
    AssistEmptyValue => "empty value" / "空值",
    AssistScopeDefault => "default" / "默认值",
    AssistScopeEnvironment => "environment" / "环境",
    AssistSavesToDefaults => "Saves to workspace defaults" / "保存到工作区默认值",
    AssistSavesToEnvironment => "Saves to the active environment" / "保存到当前环境",

    // The request pane and its dialogs.
    RequestSaveAsNew => "Save as new request" / "另存为新请求",
    RequestHistoryNotLinked => "This history snapshot is not linked to a saved request." / "该历史记录未关联已保存的请求。",
    RequestSaveTitle => "Save request" / "保存请求",
    RequestSaveSubtitle => "Name this request, then choose where to save it." / "为此请求命名，然后选择保存位置。",
    RequestRevertQuestion => "Discard edits since opening?" / "放弃打开后的修改？",
    RequestRevert => "Revert" / "还原",
    RequestNoCollection => "Create a collection to save this request." / "请先创建集合，以保存此请求。",
    RequestShowScript => "Show script preview" / "显示脚本预览",
    RequestHideScript => "Hide script preview" / "隐藏脚本预览",

    // The resolved-request inspector.
    RequestInspect => "Inspect resolved request" / "查看实际请求",
    InspectTitle => "Resolved request" / "实际请求",
    InspectSubtitle => "What Send would put on the wire; credential values are hidden." / "发送时实际发出的内容；凭据值已隐藏。",
    InspectPreparing => "Preparing…" / "正在准备…",
    InspectPreparingDetail => "Reading the draft, the variables, and the upload files." / "正在读取草稿、变量与上传文件。",
    InspectSectionRequest => "Request" / "请求",
    InspectSectionHeaders => "Headers" / "请求头",
    InspectSectionBody => "Body" / "正文",
    InspectSectionCookies => "Cookies" / "Cookie",
    InspectSectionProblems => "Problems" / "问题",
    InspectMethod => "Method" / "方法",
    InspectUrl => "URL" / "地址",
    InspectSubprotocols => "Subprotocols" / "子协议",
    InspectOriginUser => "user" / "用户",
    InspectOriginAuth => "authentication" / "认证",
    InspectOriginGenerated => "generated" / "自动生成",
    InspectBodyNone => "No body" / "无正文",
    InspectBodyJson => "JSON" / "JSON",
    InspectBodyText => "Text" / "文本",
    InspectBodyForm => "Form" / "表单",
    InspectBodyFile => "File" / "文件",
    InspectBodyMultipart => "Multipart" / "多部分",
    InspectJarNone => "The jar adds no cookies." / "Cookie 存储不会附加 Cookie。",
    InspectJarSuppressed => "A manual Cookie header suppresses the jar." / "手写的 Cookie 请求头会取代 Cookie 存储。",
    InspectUnresolvedTitle => "Unresolved variables" / "未解析的变量",
    InspectUnresolvedDetail => "The request cannot be sent until these have values." / "在填写这些值之前无法发送。",
    InspectReady => "Send would accept this request." / "发送将接受此请求。",
    InspectCopy => "Copy request" / "复制请求",
    InspectProblemEmptyUrl => "The URL is empty." / "地址为空。",

    // The response pane.
    ResponseSectionLabel => "RESPONSE" / "响应",
    ResponseIdleTitle => "Send a request to see the response" / "发送请求以查看响应",
    ResponseIdleDetail => "Nothing is sent until you press Send." / "按下发送之前不会发送任何内容。",
    ResponseNoResponseYet => "No response yet" / "尚无响应",
    ResponseSending => "Sending…" / "发送中…",
    ResponseStreaming => "Streaming…" / "流式接收中…",
    ResponseInFlightEyebrow => "In flight" / "进行中",
    ResponseSendingRequest => "Sending request…" / "正在发送请求…",
    ResponseWaitingServer => "Waiting for the server to respond." / "正在等待服务器响应。",
    ResponseSendingNewRequest => "Sending a new request…" / "正在发送新请求…",
    ResponseFailedEyebrow => "Request failed" / "请求失败",
    ResponseFailedTitle => "The request did not complete" / "请求未能完成",
    ResponseSendAgain => "Send again" / "重新发送",
    ResponseCancelledTitle => "Request cancelled" / "请求已取消",
    ResponseNoResponseReceived => "No response was received." / "未收到任何响应。",
    PreviewHistoryTruncated => "Stored history body is limited to the first 512 KiB and the response was truncated at the 20 MiB cap. Copy body contains only the stored bytes." / "已保存的历史正文仅保留前 512 KiB，且响应已在 20 MiB 上限处被截断。复制正文只包含已保存的字节。",
    PreviewHistoryLimited => "Stored history body is limited to the first 512 KiB. Copy body contains only the stored bytes." / "已保存的历史正文仅保留前 512 KiB。复制正文只包含已保存的字节。",
    PreviewTruncatedAndLimited => "Response truncated at the 20 MiB cap; showing the first 5 MiB. Copy body contains only the retained bytes." / "响应已在 20 MiB 上限处被截断，此处显示前 5 MiB。复制正文只包含已保留的字节。",
    PreviewLimited => "Preview limited to the first 5 MiB." / "预览仅显示前 5 MiB。",
    PreviewTruncated => "Response body was truncated at the 20 MiB cap; Copy body contains only the retained bytes." / "响应正文已在 20 MiB 上限处被截断；复制正文只包含已保留的字节。",

    // Workspace dialogs and the workspace menu.
    WorkspaceNoWorkspace => "No workspace" / "无工作区",
    WorkspaceOpenMenu => "Open Workspace…" / "打开工作区…",
    WorkspaceNewMenu => "New Workspace…" / "新建工作区…",
    WorkspaceReveal => "Show in File Manager" / "在文件管理器中显示",
    WorkspaceNewTitle => "New Workspace" / "新建工作区",
    WorkspaceNewDetail => "Posthaste will create a folder with this name and open it." / "Posthaste 将以此名称创建文件夹并打开它。",
    WorkspaceCreateIn => "Create in" / "创建位置",
    CommonBrowse => "Browse…" / "浏览…",
    CommonCreate => "Create" / "创建",
    CommonCreateInNewWindow => "Create in New Window" / "在新窗口中创建",
    WorkspaceInitTitle => "Create a workspace here?" / "在此创建工作区？",
    WorkspaceInitDetail => "This folder has no workspace yet. Posthaste will add its workspace files and open it." / "此文件夹尚不是工作区。Posthaste 将添加工作区文件并打开它。",
    CommonInitialize => "Initialize" / "初始化",
    CommonInitializeInNewWindow => "Initialize in New Window" / "在新窗口中初始化",
    WorkspaceOpenTitle => "Open this workspace?" / "打开此工作区？",
    WorkspaceOpenDetail => "Open it here, replacing this window, or in a new window that leaves this one as it is." / "在此打开并替换当前窗口，或在新窗口中打开并保留当前窗口。",
    CommonOpenInNewWindow => "Open in New Window" / "在新窗口中打开",
    CommonOpenHere => "Open Here" / "在此打开",

    // The tab strip.
    TabMenuCloseOthers => "Close others" / "关闭其他标签页",
    TabMenuCloseLeft => "Close to the left" / "关闭左侧标签页",
    TabMenuCloseRight => "Close to the right" / "关闭右侧标签页",
    TabMenuSaveAs => "Save as…" / "另存为…",
    TabScrollHint => "Scroll to see more tabs" / "滚动以查看更多标签页",
    TabNew => "New tab" / "新建标签页",
    TabTooltipRunning => "Sending request…" / "正在发送请求…",
    TabTooltipConnected => "WebSocket connected" / "WebSocket 已连接",
    TabTooltipFailed => "Request failed — open the tab to see why" / "请求失败 — 打开标签页查看原因",
    TabTooltipSaved => "Saved" / "已保存",
    TabTooltipUnsaved => "Unsaved changes — saves automatically" / "有未保存的更改 — 会自动保存",
    TabTooltipDraft => "Draft is not stored — Ctrl+S saves it" / "草稿尚未保存 — 按 Ctrl+S 保存",
    CloseTabHeading => "Close tab?" / "关闭标签页？",
    CloseTabConfirm => "Discard and close" / "丢弃并关闭",

    // The welcome screen.
    WelcomeTitle => "Welcome" / "欢迎",
    WelcomeSubtitle => "Open a request from the sidebar, or start a new one." / "从侧边栏打开请求，或新建一个请求。",
    WelcomeCreateWorkspace => "Create Workspace Here" / "在此创建工作区",
    WelcomeOpenWorkspace => "Open Another Workspace…" / "打开其他工作区…",
    WelcomeCreatableHint => "Nothing was written there. Creating a workspace adds workspace.json, collections/, and environments/." / "尚未写入任何内容。创建工作区会添加 workspace.json、collections/ 和 environments/。",
    WelcomeFixPathHint => "Choose another workspace folder, or fix the path named above." / "请选择其他工作区文件夹，或修复上面提到的路径。",
    WelcomeUnavailableTitle => "This workspace could not be opened" / "无法打开此工作区",
    WelcomeNewRequest => "New request" / "新建请求",
    WelcomeNewCollection => "New collection" / "新建集合",
    WelcomeRecent => "Recent" / "最近",
    WelcomeSavedRequests => "Saved requests" / "已保存的请求",
    WelcomeHistory => "History" / "历史",

    // The script preview.
    ScriptTitle => "Request script" / "请求脚本",
    ScriptCopy => "Copy script" / "复制脚本",

    // The response viewer.
    ViewPretty => "Pretty" / "美化",
    ViewTree => "Tree" / "树形",
    ViewTable => "Table" / "表格",
    ViewRaw => "Raw" / "原始",
    ViewFrames => "Frames" / "帧",
    SearchKeysPlaceholder => "Search keys and values" / "搜索键和值",
    SearchBodyPlaceholder => "Find in body" / "在正文中查找",
    CommonCopy => "Copy" / "复制",
    CommonCopied => "Copied" / "已复制",
    TreeCopyPath => "Copy path" / "复制路径",
    ResponseReceiving => "Receiving the response…" / "正在接收响应…",
    ResponseCopyBody => "Copy body" / "复制正文",
    ResponseExpandAll => "Expand all" / "全部展开",
    ResponseCollapseAll => "Collapse all" / "全部折叠",
    NoticeLimit => "Limit" / "限制",
    NoticeWarning => "Warning" / "警告",
    NoticePreview => "Preview" / "预览",
    ResponseHeadersEmpty => "This response has no headers" / "此响应没有请求头",
    ResponseCookiesWaiting => "Cookies appear when the response completes" / "响应完成后才会显示 Cookie",
    ResponseCookiesEmpty => "This response set no cookies" / "此响应未设置任何 Cookie",
    CookiesHistoryValuesNote => "Cookie values are not kept in history." / "历史记录不保留 Cookie 值。",
    CookieColumnState => "State" / "状态",
    CookieStateStored => "Stored" / "已存储",
    CookieStateRemoved => "Removed" / "已移除",
    CookieStateRejected => "Rejected" / "已拒绝",
    CookieValueNotKept => "not kept" / "未保留",
    CookieHostOnly => "Host-only" / "仅主机",
    StreamPreviewLimited => "The live preview shows the first 512 KiB. The full body appears when the response completes." / "实时预览仅显示前 512 KiB，响应完成后会显示完整正文。",
    ParseLimitedHistory => "This history entry stores only the first 512 KiB, so the JSON is incomplete. Raw shows the stored text; Copy body contains only the stored bytes." / "此历史记录仅保存前 512 KiB，因此 JSON 不完整。原始视图显示已保存的文本；复制正文只包含已保存的字节。",
    ParseLimitedLive => "This body is larger than the 5 MiB preview, so the JSON is incomplete. Raw shows the first 5 MiB; Copy body contains the full retained body." / "此正文大于 5 MiB 的预览上限，因此 JSON 不完整。原始视图显示前 5 MiB；复制正文包含全部已保留的正文。",
    InvalidJson => "This body does not parse as a JSON object or array, so it is shown as raw text." / "此正文无法解析为 JSON 对象或数组，因此以原始文本显示。",
    TableUnavailable => "This body has no table to show. The Table view needs a non-empty array of objects." / "此正文没有可显示的表格。表格视图需要由对象组成的非空数组。",
    FormattedLimited => "The formatted view is limited to the first 5 MiB. Raw shows the original preview." / "美化视图仅显示前 5 MiB。原始视图显示原始预览。",
    ResponseBodyEmpty => "This response has no body" / "此响应没有正文",
    ResponseBinaryTitle => "Binary response" / "二进制响应",
    ResponseBinaryStreaming => "Receiving a binary response…" / "正在接收二进制响应…",
    ResponseBinaryUnknownType => "unknown type" / "未知类型",
    ResponseBinaryTruncated => "The response was truncated; a saved file will be incomplete." / "响应已被截断，保存的文件将不完整。",
    ResponseBinaryNotStored => "History keeps binary metadata only, so this body is not available." / "历史记录仅保留二进制元数据，无法提供正文。",
    ResponseSaveBody => "Save body…" / "保存正文…",
    FramesEmpty => "This event stream has no frames" / "此事件流没有帧",
    ResponseParsing => "Parsing large response…" / "正在解析较大的响应…",

    // The request editor.
    TabParams => "Params" / "参数",
    TabAuth => "Auth" / "认证",
    TabHeaders => "Headers" / "请求头",
    TabBody => "Body" / "正文",
    TabResponseHeaders => "Headers" / "响应头",
    TabOptions => "Options" / "选项",

    CommonNone => "None" / "无",

    BodyText => "Text" / "文本",
    BodyForm => "Form" / "表单",
    BodyFile => "File" / "文件",
    BodyMultipart => "Multipart" / "多部分",
    // A media type's name is written the same way in both languages.
    BodyJson => "JSON" / "JSON",

    AuthBearer => "Bearer Token" / "Bearer 令牌",
    AuthBasic => "Basic Auth" / "Basic 认证",
    AuthApiKey => "API Key" / "API 密钥",

    AuthFieldToken => "Token" / "令牌",
    AuthFieldUsername => "Username" / "用户名",
    AuthFieldPassword => "Password" / "密码",
    AuthFieldKey => "Key" / "键",
    AuthLocationHeader => "Header" / "请求头",
    AuthLocationQuery => "Query" / "查询参数",

    RowHeaderName => "Header name" / "请求头名称",
    ResponseHeaderName => "Header name" / "响应头名称",
    RowParameter => "Parameter" / "参数",
    RowField => "Field" / "字段",
    ColumnName => "Name" / "名称",
    ColumnValue => "Value" / "值",
    ColumnPath => "Path" / "路径",
    ColumnExpires => "Expires" / "过期时间",
    ColumnFlags => "Flags" / "属性",

    EditorAddTo => "Add to" / "添加到",
    EditorAddHeader => "+ Add header" / "+ 添加请求头",
    EditorAddParameter => "+ Add parameter" / "+ 添加参数",
    EditorAddField => "+ Add field" / "+ 添加字段",
    EditorAddFile => "+ Add file" / "+ 添加文件",
    EditorChooseFile => "Choose…" / "选择…",
    EditorNoFile => "No file chosen" / "未选择文件",
    EditorMissingFile => "File not found" / "文件不存在",
    EditorFormat => "Format" / "格式化",
    EditorGeneratedHeaders => "Generated at send time" / "发送时生成",
    EditorAutoTag => "Auto" / "自动",
    EditorRequestBody => "Request body" / "请求正文",
    EditorRequestTimeout => "Request timeout" / "请求超时",
    EditorRequestTimeoutHint => "Milliseconds. Leave empty to use the app default." / "毫秒。留空则使用应用默认值。",
    EditorConnectTimeout => "Connection timeout" / "连接超时",
    EditorConnectTimeoutHint => "Milliseconds. Bounds the opening handshake only; an open session is not closed when it elapses." / "毫秒。仅限制握手阶段；会话建立后不会因超时关闭。",
    EditorSubprotocols => "Subprotocols" / "子协议",
    EditorSubprotocolsHint => "Comma-separated. Offered in order on the handshake; empty entries are ignored." / "以逗号分隔。握手时按顺序提供，空项会被忽略。",

    // The command bar's transport entry.
    ProtocolWebSocket => "WebSocket" / "WebSocket",
    ProtocolWebSocketShort => "WS" / "WS",

    // The WebSocket session pane. The region label follows the pane-header
    // convention other panels use (RESPONSE, NAME, VALUE): an uppercase
    // eyebrow, because it names a region rather than the technology.
    WsSectionLabel => "WEBSOCKET" / "WEBSOCKET",
    WsNoSession => "No session is open" / "尚未建立会话",
    WsNoSessionHint => "Connect from the command bar to open a WebSocket session." / "从命令栏连接以打开 WebSocket 会话。",
    WsConnecting => "Connecting…" / "正在连接…",
    WsConnectingHint => "Waiting for the server to accept the handshake." / "正在等待服务器接受握手。",
    WsConnected => "Connected" / "已连接",
    WsClosed => "Session closed" / "会话已关闭",
    WsClosedByUser => "Closed by you" / "由你关闭",
    WsClosedByServer => "Closed by the server" / "由服务器关闭",
    WsCloseCode => "Close code" / "关闭代码",
    WsCloseReason => "Reason" / "原因",
    WsFailed => "Connection failed" / "连接失败",
    WsFailedHint => "The draft is unchanged. Connect again from the command bar." / "草稿未更改，可从命令栏重新连接。",
    WsSubprotocol => "Subprotocol" / "子协议",
    WsMessages => "Messages" / "消息",
    WsFramesEmpty => "No frames yet" / "尚无帧",
    WsEmptyPayload => "Empty frame" / "空帧",
    WsClearLog => "Clear log" / "清空记录",
    WsTruncated => "The frame log reached its limit. Later frames are not shown; the session is still open." / "帧记录已达上限，后续帧不再显示；会话仍保持连接。",
    WsDirectionIncoming => "Received" / "接收",
    WsDirectionOutgoing => "Sent" / "发送",
    WsSendMessage => "Send message" / "发送消息",
    WsComposerPlaceholder => "Message" / "消息",
    WsComposerSendsWhenConnected => "Sending is available while the session is connected." / "仅在会话连接时才能发送。",
    WsCopyPayload => "Copy payload" / "复制载荷",
    WsPayloadNotRetained => "This frame is not retained in full." / "此帧未被完整保留。",
    EditorTimeoutOutOfRange => "Timeout must be greater than zero." / "超时必须大于零。",
    EditorTimeoutNotANumber => "Timeout must be a positive number of milliseconds." / "超时必须是表示毫秒的正整数。",

    // The Cookies dialog.
    CookiesTitle => "Cookies" / "Cookie",
    CookieAdd => "Add cookie" / "添加 Cookie",
    CookieAddOpen => "+ Add cookie" / "+ 添加 Cookie",
    CookieClearAll => "Clear all" / "清除全部",
    CookieAddHint => "Stores a host-only session cookie. It is never written into the request or the workspace." / "保存一个仅限主机的会话 Cookie，不会写入请求或工作区。",
    CookieFieldHost => "Host" / "主机",
    CookieNameRequired => "Enter a cookie name." / "请输入 Cookie 名称。",
    CookieHostRequired => "Enter a host." / "请输入主机。",
    CookiePathMustBeAbsolute => "Path must start with /." / "路径必须以 / 开头。",
    CookieEmptyTitle => "No cookies stored" / "尚无 Cookie",
    CookieEmptyDetail => "Send a request that sets a cookie, or use Add cookie." / "发送一个会设置 Cookie 的请求，或使用添加 Cookie。",
    CookiesManualHeaderHint => "An enabled Cookie header row replaces the jar for this request, so automatic cookies are not attached." / "启用的 Cookie 请求头行会替代 Cookie 存储，本请求不再附带自动 Cookie。",
    CookiesPersistenceSaved => "Saved for this workspace." / "已为此工作区保存。",
    CookieClearDetail => "This removes every stored cookie and deletes this workspace's saved cookie file. Other workspaces keep theirs." / "这会移除所有已存储的 Cookie，并删除此工作区保存的 Cookie 文件；其他工作区不受影响。",
    CookieSession => "Session" / "会话",
    // Cookie attribute names are protocol tokens.
    CookieFlagSecure => "Secure" / "Secure",
    CookieFlagHttpOnly => "HttpOnly" / "HttpOnly",
    CookieAttributeSameSite => "SameSite" / "SameSite",
}

message_table! {
    StatusSaveFailed => "Save failed: {}" / "保存失败：{}",
    // A save that was refused without a workspace status to quote. The dialog
    // normally shows the status bar's own sentence; this keeps it from closing
    // silently if that status is ever missing.
    SavePickerWriteFailed => "The request could not be saved." / "无法保存此请求。",
    SettingsWriteFailed => "Preferences write failed: {}" / "偏好设置写入失败：{}",

    // Workspace notices.
    LastWorkspaceUnavailable => "The last workspace {} could not be opened; showing the default workspace instead." / "上次使用的工作区 {} 无法打开，已改为显示默认工作区。",
    WorkspaceFolderOpenFailed => "The workspace folder {} could not be opened: {}" / "无法打开工作区文件夹 {}：{}",
    WorkspaceGone => "The workspace {} is no longer available." / "工作区 {} 已不可用。",
    WorkspaceReadFailed => "The workspace {} could not be read: {}" / "无法读取工作区 {}：{}",
    WorkspaceCreateFailed => "The workspace at {} could not be created: {}" / "无法在 {} 创建工作区：{}",
    WorkspaceLaunchFailed => "Posthaste could not start on {}: {}" / "Posthaste 无法在 {} 启动：{}",

    // Import and export notices.
    ImportFailed => "Import failed: {}" / "导入失败：{}",
    NothingImportedBundle => "Nothing was imported from a Posthaste bundle; the document held no collections." / "未从 Posthaste 打包文件导入任何内容；文档中没有任何集合。",
    NothingImportedPostman => "Nothing was imported from a Postman collection; the document held no collections." / "未从 Postman 集合导入任何内容；文档中没有任何集合。",
    NothingImported => "Nothing was imported; the document held no collections." / "未导入任何内容；文档中没有任何集合。",
    ImportedFromBundle => "Imported {} from a Posthaste bundle." / "已从 Posthaste 打包文件导入 {}。",
    ImportedFromPostman => "Imported {} from a Postman collection." / "已从 Postman 集合导入 {}。",
    Imported => "Imported {}." / "已导入 {}。",
    ImportSkipped => "Skipped {}: {}" / "已跳过 {}：{}",
    ImportSkippedMore => "and {} more." / "另有 {} 项。",
    ImportReadFailed => "the file could not be read: {}" / "无法读取文件：{}",

    ExportBundlePrompt => "Export \"{}\" as a Posthaste bundle?" / "将“{}”导出为 Posthaste 打包文件？",
    ExportPostmanPrompt => "Export \"{}\" as a Postman collection?" / "将“{}”导出为 Postman 集合？",
    ExportFailed => "Could not export: {}" / "导出失败：{}",
    ExportedCollection => "Exported {} to {}." / "已将 {} 导出到 {}。",
    ExportCollectionUnchanged => "Could not export {}: {}. The collection is unchanged." / "无法导出 {}：{}。集合未作更改。",

    // The request script notice.
    ScriptMissingVariable => "Missing variable: {}." / "缺少变量：{}。",
    ScriptMissingVariables => "Missing variables: {}." / "缺少变量：{}。",
    ScriptBodyTruncated => "The body is longer than {} KiB; the preview shows its start." / "正文超过 {} KiB，预览仅显示开头部分。",
    MissingVariables => "Missing variables: {}" / "缺少变量：{}",

    // The resolved-request inspector.
    InspectBodySize => "{} bytes" / "{} 字节",
    InspectUploadPath => "Upload path: {}" / "上传路径：{}",
    InspectJarAttached => "The jar adds: {}" / "Cookie 存储会附加：{}",
    InspectProblemInvalidUrl => "The URL is not valid: {}" / "地址无效：{}",
    InspectProblemInvalidHeader => "The header {} is not valid: {}" / "请求头 {} 无效：{}",
    InspectProblemInvalidJson => "The body is not valid JSON: {}" / "正文不是有效的 JSON：{}",
    InspectProblemNoFile => "No file is chosen for {}." / "{} 未选择文件。",
    InspectProblemNotAFile => "{} is not a file." / "{} 不是文件。",
    InspectProblemUpload => "{} cannot be read: {}" / "无法读取 {}：{}",

    // The response pane's banners.
    ResponseHistoryBanner => "HISTORY · {}" / "历史 · {}",
    WsFrameTotals => "{} sent · {} received · {}" / "已发送 {} · 已接收 {} · {}",
    WsTrafficSummary => "{} frames and {} bytes sent; {} frames and {} bytes received" / "已发送 {} 帧、{} 字节；已接收 {} 帧、{} 字节",

    // The response viewer's counts and notices.
    ScriptCookiesButton => "Cookies ({})" / "Cookie（{}）",
    TabTooltipSaveFailed => "Save failed: {} — Ctrl+S retries" / "保存失败：{} — 按 Ctrl+S 重试",
    HistoryUnavailable => "History unavailable: {}" / "历史记录不可用：{}",
    HistoryWriteFailed => "The history entry could not be written: {}" / "无法写入历史记录：{}",
    HistoryPruneFailed => "An old history entry could not be deleted: {}" / "无法删除旧的历史记录：{}",
    HistoryDeleteFailed => "The history entry could not be deleted: {}" / "无法删除历史记录：{}",
    HistoryClearFailed => "The history could not be cleared: {}" / "无法清空历史记录：{}",
    CloseTabsHeading => "Close {} tabs?" / "关闭 {} 个标签页？",
    CloseTabsMessage => "Unsaved drafts in {} of {} tabs will be discarded." / "{} / {} 个标签页中有未保存的草稿，将被丢弃。",
    CloseTabMessage => "Discard the unsaved draft in \"{}\"?" / "丢弃「{}」中未保存的草稿？",
    MatchCounter => "{} of {}" / "第 {} 项，共 {} 项",
    FramesLiveLimited => "The frames view is limited to {} frames or {} MiB of frame text. Raw shows the received text." / "帧视图仅显示前 {} 帧或 {} MiB 的帧文本。原始视图显示已接收的文本。",
    FramesPreviewLimited => "The frames view is limited to {} frames or {} MiB of frame text. Raw shows the preview text." / "帧视图仅显示前 {} 帧或 {} MiB 的帧文本。原始视图显示预览文本。",
    TreeLimited => "Tree is limited to the first {} nodes. Pretty or Raw shows the rest." / "树形视图仅显示前 {} 个节点。美化或原始视图显示其余内容。",
    TableRowsLimited => "{} of {} rows" / "{} / {} 行",
    TableColumnsLimited => "{} of {} columns" / "{} / {} 列",
    TableLimited => "The table is limited to {}. Pretty or Raw shows the rest." / "表格仅显示 {}。美化或原始视图显示其余内容。",
    ResponseBinaryMediaType => "Media type: {}" / "类型：{}",
    ResponseBinarySize => "Size: {}" / "大小：{}",
    ResponseBinaryFileName => "Suggested name: {}" / "建议文件名：{}",
    ResponseBodySaved => "Saved to {}" / "已保存到 {}",
    ResponseBodySavedPartial => "Saved the retained prefix to {}; the response was truncated." / "已保存保留的前缀到 {}；响应已被截断。",
    ResponseBodySaveFailed => "Could not save: {}" / "保存失败：{}",

    // The request editor's dialogs and validation.
    CookiesSummarySent => "{} stored · {} sent with this request" / "已存储 {} · 本请求将发送 {}",
    CookiesSummaryManualHeader => "{} stored · the request's own Cookie header replaces the jar" / "已存储 {} · 请求自身的 Cookie 请求头会替代 Cookie 存储",
    CookiesSummaryNoUrl => "{} stored · no request URL" / "已存储 {} · 尚无请求地址",
    CookiesPersistenceSession => "Session only · {}" / "仅限本次会话 · {}",
    CookiesPersistenceFailed => "Could not save the cookie file: {}" / "无法保存 Cookie 文件：{}",
    CookieClearQuestion => "Clear all {}?" / "清除全部 {}？",
    CookieStoreFailed => "Could not store the cookie: {}" / "无法存储 Cookie：{}",
    CookieHostInvalid => "{} is not a valid host." / "{} 不是有效的主机。",

    // The delete confirmation.
    DeleteTabsNoteOne => "1 open tab will keep its contents as an unsaved draft." / "有 1 个打开的标签页，其内容将保留为未保存的草稿。",
    DeleteTabsNoteMany => "{} open tabs will keep their contents as unsaved drafts." / "有 {} 个打开的标签页，其内容将保留为未保存的草稿。",
    DeleteNamed => "Delete \"{}\"?" / "删除“{}”？",
    DeleteWithOne => "Delete \"{}\" and its {}?" / "删除“{}”及其 {}？",
    DeleteWithTwo => "Delete \"{}\" and its {} and {}?" / "删除“{}”及其 {}和{}？",
    StorageIssueLine => "{}: {}" / "{}：{}",

    // Save failures. A failed save is one sentence, and the count of documents
    // that could not be written uses the counted form.
    CollectionNotLoaded => "collection {} is not loaded" / "集合 {} 未加载",
    SaveDocumentsFailed => "{} could not be written" / "{}未能写入",

    // Storage reports. Each is one sentence with the storage detail as its
    // argument; the detail itself stays in the layer that produced it.
    StorageCollectionNameFailed => "The collection directory name could not be computed: {}" / "无法计算集合目录名：{}",
    StorageCollectionMoveFailed => "The collection directory could not be moved: {}" / "集合目录无法移动：{}",
    StorageCollectionMoveBackFailed => "The collection directory could not be moved back after a failed save: {}" / "保存失败后，集合目录无法移回：{}",
    StorageEnvironmentNameFailed => "The environment file name could not be computed: {}" / "无法计算环境文件名：{}",
    StorageEnvironmentMoveFailed => "The environment file could not be moved: {}" / "环境文件无法移动：{}",
    StorageRequestCopyFailed => "The request copy could not be written: {}" / "请求副本无法写入：{}",
    StoragePreservedDeleteFailed => "The preserved document could not be deleted: {}" / "无法删除保留的文档：{}",

    // Workspace creation and export.
    WorkspaceTargetNotEmpty => "{}: the folder is not empty" / "{}：文件夹不为空",
    RequestReadFailed => "{} could not be read: {}" / "无法读取 {}：{}",
}

/// Keys whose two texts are identical on purpose, in declaration order.
///
/// The catalogue test asserts this is exactly the set of entries that read the
/// same in both languages, so an accidental missing translation cannot hide
/// behind the list.
#[cfg(test)]
pub const INVARIANT_KEYS: &[&str] = &[
    "LanguageEnglish",
    "LanguageChinese",
    "BodyJson",
    // Protocol tokens: the WebSocket label, its short form, and the script
    // section heading are written the same way in both languages.
    "ProtocolWebSocket",
    "ProtocolWebSocketShort",
    "WsSectionLabel",
    "CookieFlagSecure",
    "CookieFlagHttpOnly",
    "CookieAttributeSameSite",
];

/// Counts whose noun and measure word belong to the language.
///
/// English pluralizes; Chinese keeps the noun and puts the count first. A
/// caller never concatenates a number and a noun itself, so a translated
/// sentence stays grammatical.
pub mod counted {
    use super::super::{Lang, current};

    /// A workspace object a count can name.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Item {
        Collection,
        Folder,
        Request,
        Environment,
        /// A stored document written by the save path. Only used by the
        /// failed-write sentence, so it is not an object the UI lists.
        Document,
    }

    impl Item {
        fn english(self, plural: bool) -> &'static str {
            match (self, plural) {
                (Self::Collection, false) => "collection",
                (Self::Collection, true) => "collections",
                (Self::Folder, false) => "folder",
                (Self::Folder, true) => "folders",
                (Self::Request, false) => "request",
                (Self::Request, true) => "requests",
                (Self::Environment, false) => "environment",
                (Self::Environment, true) => "environments",
                (Self::Document, false) => "document",
                (Self::Document, true) => "documents",
            }
        }

        fn chinese(self) -> &'static str {
            match self {
                Self::Collection => "集合",
                Self::Folder => "文件夹",
                Self::Request => "请求",
                Self::Environment => "环境",
                Self::Document => "文档",
            }
        }
    }

    /// `1 request`, `3 requests`, or `3 个请求`.
    pub fn item(item: Item, count: usize) -> String {
        match current() {
            Lang::English => format!("{count} {}", item.english(count != 1)),
            Lang::Chinese => format!("{count} 个{}", item.chinese()),
        }
    }

    /// Joins already-counted phrases the way the language lists them.
    pub fn list(parts: &[String]) -> String {
        match (current(), parts) {
            (_, []) => String::new(),
            (_, [only]) => only.clone(),
            (Lang::English, [first, second]) => format!("{first} and {second}"),
            (Lang::English, _) => {
                let (last, rest) = parts.split_last().expect("two or more entries");
                format!("{}, and {last}", rest.join(", "))
            }
            (Lang::Chinese, [first, second]) => format!("{first}和{second}"),
            (Lang::Chinese, _) => {
                let (last, rest) = parts.split_last().expect("two or more entries");
                format!("{}和{last}", rest.join("、"))
            }
        }
    }

    /// `1 key`, `3 keys`, or `3 个键`.
    pub fn keys(count: usize) -> String {
        match current() {
            Lang::English => format!("{count} key{}", if count == 1 { "" } else { "s" }),
            Lang::Chinese => format!("{count} 个键"),
        }
    }

    /// `1 item`, `3 items`, or `3 项`.
    pub fn items(count: usize) -> String {
        match current() {
            Lang::English => format!("{count} item{}", if count == 1 { "" } else { "s" }),
            Lang::Chinese => format!("{count} 项"),
        }
    }

    /// `1 cookie`, `3 cookies`, or `3 个 Cookie`.
    pub fn cookie(count: usize) -> String {
        match current() {
            Lang::English => {
                if count == 1 {
                    "1 cookie".to_owned()
                } else {
                    format!("{count} cookies")
                }
            }
            Lang::Chinese => format!("{count} 个 Cookie"),
        }
    }
}
