unit BridgeManagerMain;

interface

uses
  Winapi.Windows,
  Winapi.Messages,
  Winapi.CommCtrl,
  Winapi.UxTheme,
  Winapi.ShellAPI,
  System.SysUtils,
  System.Classes,
  System.Generics.Collections,
  System.JSON,
  System.UITypes,
  System.Math,
  System.IOUtils,
  System.Net.URLClient,
  System.Net.HttpClient,
  Vcl.Forms,
  Vcl.Controls,
  Vcl.StdCtrls,
  Vcl.ComCtrls,
  Vcl.ExtCtrls,
  Vcl.Graphics,
  Vcl.Dialogs;

type
  TProfileRow = record
    Model: string;
    BaseUrl: string;
    Proxy: string;
    Route: string;
    FileName: string;
    Active: Boolean;
  end;

  TStatusSnapshot = record
    Online: Boolean;
    HasProfile: Boolean;
    Model: string;
    FileName: string;
    Transport: string;
    ThinkingLevel: string;
    ThinkingChoices: string;
    ThinkingLocked: Boolean;
    ProviderReasoningSupported: Boolean;
    ProviderReasoningChoices: string;
    ProviderReasoningCurrent: string;
    Stamp: string;
    SettingsDir: string;
    Detail: string;
    LsControls: string;
  end;

  TMainForm = class(TForm)
  private
    FBridgeUrl: string;
    FLocalAuthToken: string;
    FProfileFiles: TStringList;
    FActiveFile: string;
    FBridgeRoot: string;
    FTopPanel: TPanel;
    FAccentPanel: TPanel;
    FStatusPanel: TPanel;
    FStatusDotLabel: TLabel;
    FTitleLabel: TLabel;
    FStatusLabel: TLabel;
    FCurrentModelLabel: TLabel;
    FHintLabel: TLabel;
    FLsPanel: TPanel;
    FLsModel, FLsReasoning: TComboBox;
    FLsGroups: string;
    FLastLsControls: string;
    FLsCurrentModel: string;
    FUpdatingLs: Boolean;
    FThinkingGroup: TRadioGroup;
    FProviderReasoningGroup: TRadioGroup;
    FProviderReasoningChoices: TStringList;
    FProviderReasoningCurrent: string;
    FProviderReasoningSupported: Boolean;
    FUpdatingProviderReasoningUi: Boolean;
    FContentPanel: TPanel;
    FListPanel: TPanel;
    FListTitleLabel: TLabel;
    FListHintLabel: TLabel;
    FTestResultLabel: TLabel;
    FProfileList: TListView;
    FLogPanel: TPanel;
    FLogTitleLabel: TLabel;
    FLogMemo: TMemo;
    FLogSplitter: TSplitter;
    FStatusBar: TStatusBar;
    FRefreshButton: TButton;
    FSwitchButton: TButton;
    FTestButton: TButton;
    FStartButton: TButton;
    FStopButton: TButton;
    FPollTimer: TTimer;
    FClosing: Boolean;
    FBusy: Boolean;
    FTestInFlight: Boolean; // 连通性测试在途：只禁用“测试”按钮，不阻塞窗口
    FStatusInFlight: Boolean;
    FProfilesBusy: Boolean;
    FProfilesLoaded: Boolean;
    FSettingsStamp: string;
    FThinkingLevel: string;
    FThinkingLocked: Boolean;
    FUpdatingThinkingUi: Boolean;
    FWorkerCount: Integer;
    procedure ApplyLsControls(const ASnapshot: TStatusSnapshot);
    procedure LsModelChange(Sender: TObject);
    procedure LsReasoningChange(Sender: TObject);
    procedure SetLsModelAsync(const AModel: string);
    procedure FormShown(Sender: TObject);
    procedure FormResize(Sender: TObject);
    procedure RefreshClick(Sender: TObject);
    procedure SwitchClick(Sender: TObject);
    procedure TestSelectedClick(Sender: TObject);
    procedure StartClick(Sender: TObject);
    procedure StopClick(Sender: TObject);
    procedure ThinkingLevelClick(Sender: TObject);
    procedure ProviderReasoningClick(Sender: TObject);
    procedure PollTimer(Sender: TObject);
    procedure ProfileDblClick(Sender: TObject);
    procedure ListSelectItem(Sender: TObject; Item: TListItem;
      Selected: Boolean);
    procedure BuildUi;
    procedure ApplyWindowsAppearance;
    procedure AppendLog(const AText: string);
    procedure QueueToMain(const AProc: TProc);
    procedure SetBusy(const ABusy: Boolean);
    procedure RefreshStatusAsync(const AQuiet: Boolean);
    procedure StartProfilesRefresh(const AQuiet: Boolean);
    procedure SwitchProfileAsync(const AFileName: string);
    procedure TestProfileAsync(const AFileName: string);
    procedure SetThinkingLevelAsync(const ALevel: string);
    procedure SetProviderReasoningAsync(const ALevel: string);
    procedure StartStopAsync(const AStart: Boolean);
    procedure ApplyStatusSnapshot(const ASnapshot: TStatusSnapshot;
      const AQuiet: Boolean);
    procedure ApplyThinkingControls(const ASnapshot: TStatusSnapshot);
    procedure ApplyProviderReasoningControls(const ASnapshot: TStatusSnapshot);
    procedure FillProfileList(const ARows: TArray<TProfileRow>);
    procedure AdjustProfileColumns;
    procedure SetBridgeState(const AOnline: Boolean; const ADetail: string);
    function NewHttpClient(const AResponseTimeout: Integer): THTTPClient;
    function GetJsonWith(AClient: THTTPClient; const APath: string): TJSONObject;
    function PostJsonWith(AClient: THTTPClient; const APath,
      AJson: string): TJSONObject;
    function JsonText(AObject: TJSONObject; const AName: string): string;
    function JsonBool(AObject: TJSONObject; const AName: string): Boolean;
    function JsonArrayText(AObject: TJSONObject; const AName: string): string;
    function ProbeStageName(const AStage: string): string;
    function ProbeTransportName(const ATransport: string): string;
    function LocalizeProbeMessage(const AMessage: string): string;
    function ProbeSuccessSummary(const ATransport, AUpstreamStatus,
      ARawMessage: string): string;
    function ProbeNetworkSummary(const AProxyMode, AProxyUrl,
      AResponseHeadersMs, AFirstByteMs, AResponseBodyMs,
      ATotalMs: string): string;
    function FindBridgeRoot: string;
    function ResolveBridgeUrl: string;
    function ReadLocalAuthToken: string;
    function LocalAuthTokenPath: string;
    function ReadStateListen(const AStatePath: string): string;
    function ReadServiceStateFilePath: string;
    function RunPowerShellScript(const AScriptPath: string): Cardinal;
  protected
    procedure DoClose(var Action: TCloseAction); override;
  public
    constructor Create(AOwner: TComponent); override;
    destructor Destroy; override;
  end;

var
  MainForm: TMainForm;

implementation

const
  DEFAULT_BRIDGE_URL = 'http://127.0.0.1:18787';
  STATUS_RESPONSE_TIMEOUT_MS = 4000;
  ACTION_RESPONSE_TIMEOUT_MS = 30000;
  // 桥接器侧探测有 30 秒总超时，客户端再加 50% 余量。
  PROBE_RESPONSE_TIMEOUT_MS = 45000;
  SCRIPT_PROCESS_TIMEOUT_MS = 5 * 60 * 1000;
  LOG_MAX_LINES = 500;
  DWMWA_WINDOW_CORNER_PREFERENCE = 33;
  DWMWCP_ROUND = 2;

type
  TDwmSetWindowAttribute = function(
    hWnd: HWND;
    dwAttribute: DWORD;
    pvAttribute: Pointer;
    cbAttribute: DWORD
  ): HRESULT; stdcall;

constructor TMainForm.Create(AOwner: TComponent);
var
  LStage: string;
begin
  LStage := '创建 VCL 窗口';
  try
    inherited CreateNew(AOwner);
    LStage := '设置窗口属性';
    Caption := 'Claude Code 模型中心';
    Width := 1020;
    Height := 940;
    Position := poScreenCenter;
    Constraints.MinWidth := 900;
    Constraints.MinHeight := 820;
    Color := $00F5F5F5;
    Font.Name := 'Segoe UI';
    Font.Size := 10;
    DoubleBuffered := True;
    Icon.Assign(Application.Icon);

    LStage := '创建配置列表';
    FProfileFiles := TStringList.Create;
    FProviderReasoningChoices := TStringList.Create;
    FBridgeRoot := FindBridgeRoot;
    FBridgeUrl := ResolveBridgeUrl;
    FLocalAuthToken := ReadLocalAuthToken;

    LStage := '创建界面控件';
    BuildUi;
    OnShow := FormShown;
    OnResize := FormResize;
  except
    on E: Exception do
      raise Exception.Create(
        'TMainForm.Create/' + LStage + ': ' + E.ClassName + ': ' + E.Message
      );
  end;
end;

destructor TMainForm.Destroy;
begin
  FPollTimer.Enabled := False;
  FProviderReasoningChoices.Free;
  FProfileFiles.Free;
  inherited;
end;

procedure TMainForm.BuildUi;
var
  LColumn: TListColumn;
begin
  FTopPanel := TPanel.Create(Self);
  FTopPanel.Parent := Self;
  FTopPanel.Align := alTop;
  FTopPanel.Height := 164;
  FTopPanel.BevelOuter := bvNone;
  FTopPanel.Color := clWhite;
  FTopPanel.ParentBackground := False;

  FAccentPanel := TPanel.Create(Self);
  FAccentPanel.Parent := FTopPanel;
  FAccentPanel.Align := alTop;
  FAccentPanel.Height := 4;
  FAccentPanel.BevelOuter := bvNone;
  FAccentPanel.Color := $00D47800;
  FAccentPanel.ParentBackground := False;

  FTitleLabel := TLabel.Create(Self);
  FTitleLabel.Parent := FTopPanel;
  FTitleLabel.SetBounds(24, 20, 430, 34);
  FTitleLabel.Caption := 'Claude Code 模型中心';
  FTitleLabel.Font.Name := 'Segoe UI';
  FTitleLabel.Font.Size := 18;
  FTitleLabel.Font.Style := [fsBold];
  FTitleLabel.Font.Color := $00242424;

  FHintLabel := TLabel.Create(Self);
  FHintLabel.Parent := FTopPanel;
  FHintLabel.SetBounds(25, 57, 500, 22);
  FHintLabel.Caption := '统一管理模型路由、Provider 配置和桥接服务';
  FHintLabel.Font.Name := 'Segoe UI';
  FHintLabel.Font.Color := $00606060;

  FStatusPanel := TPanel.Create(Self);
  FStatusPanel.Parent := FTopPanel;
  FStatusPanel.SetBounds(24, 91, 430, 56);
  FStatusPanel.BevelOuter := bvNone;
  FStatusPanel.Color := $00F2F2F2;
  FStatusPanel.ParentBackground := False;

  FStatusDotLabel := TLabel.Create(Self);
  FStatusDotLabel.Parent := FStatusPanel;
  FStatusDotLabel.SetBounds(16, 14, 20, 24);
  FStatusDotLabel.Caption := '●';
  FStatusDotLabel.Font.Name := 'Segoe UI Symbol';
  FStatusDotLabel.Font.Size := 11;
  FStatusDotLabel.Font.Color := $00707070;

  FStatusLabel := TLabel.Create(Self);
  FStatusLabel.Parent := FStatusPanel;
  FStatusLabel.SetBounds(43, 8, 360, 22);
  FStatusLabel.Caption := '桥接器：检查中';
  FStatusLabel.Font.Name := 'Segoe UI';
  FStatusLabel.Font.Style := [fsBold];
  FStatusLabel.Font.Color := $00404040;

  FCurrentModelLabel := TLabel.Create(Self);
  FCurrentModelLabel.Parent := FStatusPanel;
  FCurrentModelLabel.SetBounds(43, 30, 360, 20);
  FCurrentModelLabel.Caption := '当前模型：-';
  FCurrentModelLabel.Font.Name := 'Segoe UI';
  FCurrentModelLabel.Font.Color := $00606060;

  FThinkingGroup := TRadioGroup.Create(Self);
  FThinkingGroup.Parent := FTopPanel;
  FThinkingGroup.SetBounds(596, 78, 400, 70);
  FThinkingGroup.Caption := 'Gemini Thinking（下一请求生效）';
  FThinkingGroup.Columns := 3;
  FThinkingGroup.Items.Add('低');
  FThinkingGroup.Items.Add('中');
  FThinkingGroup.Items.Add('高');
  FThinkingGroup.ItemIndex := -1;
  FThinkingGroup.Enabled := False;
  FThinkingGroup.OnClick := ThinkingLevelClick;

  // Provider Reasoning 与 Gemini Thinking 共用同一块布局，后创建因此位于
  // 上层：Qwen/DeepSeek 活动时显示并隐藏 Gemini 组，反之亦然，两个控件
  // 任何时刻都不会同时可操作。
  FProviderReasoningGroup := TRadioGroup.Create(Self);
  FProviderReasoningGroup.Parent := FTopPanel;
  FProviderReasoningGroup.SetBounds(596, 78, 400, 70);
  FProviderReasoningGroup.Caption := 'Provider Reasoning（下一请求生效）';
  FProviderReasoningGroup.Columns := 4;
  FProviderReasoningGroup.ItemIndex := -1;
  FProviderReasoningGroup.Enabled := False;
  FProviderReasoningGroup.Visible := False;
  FProviderReasoningGroup.OnClick := ProviderReasoningClick;

  FLsPanel := TPanel.Create(Self);
  FLsPanel.Parent := FTopPanel;
  FLsPanel.SetBounds(596, 78, 400, 70);
  FLsPanel.BevelOuter := bvNone;
  FLsPanel.Visible := False;
  with TLabel.Create(Self) do
  begin
    Parent := FLsPanel;
    SetBounds(0, 0, 400, 20);
    Caption := 'LS 模型 / 推理档位 · Turbo · 自动审批（新会话生效）';
  end;
  FLsModel := TComboBox.Create(Self);
  FLsModel.Parent := FLsPanel;
  FLsModel.SetBounds(0, 24, 270, 28);
  FLsModel.Style := csDropDownList;
  FLsModel.OnChange := LsModelChange;
  FLsReasoning := TComboBox.Create(Self);
  FLsReasoning.Parent := FLsPanel;
  FLsReasoning.SetBounds(280, 24, 120, 28);
  FLsReasoning.Style := csDropDownList;
  FLsReasoning.OnChange := LsReasoningChange;

  FRefreshButton := TButton.Create(Self);
  FRefreshButton.Parent := FTopPanel;
  FRefreshButton.SetBounds(572, 24, 96, 34);
  FRefreshButton.Caption := '刷新配置';
  FRefreshButton.OnClick := RefreshClick;

  FSwitchButton := TButton.Create(Self);
  FSwitchButton.Parent := FTopPanel;
  FSwitchButton.SetBounds(676, 24, 132, 34);
  FSwitchButton.Caption := '切换选中模型';
  FSwitchButton.Default := True;
  FSwitchButton.OnClick := SwitchClick;

  FStartButton := TButton.Create(Self);
  FStartButton.Parent := FTopPanel;
  FStartButton.SetBounds(816, 24, 86, 34);
  FStartButton.Caption := '启动服务';
  FStartButton.OnClick := StartClick;

  FStopButton := TButton.Create(Self);
  FStopButton.Parent := FTopPanel;
  FStopButton.SetBounds(910, 24, 86, 34);
  FStopButton.Caption := '停止服务';
  FStopButton.OnClick := StopClick;

  FStatusBar := TStatusBar.Create(Self);
  FStatusBar.Parent := Self;
  FStatusBar.Align := alBottom;
  FStatusBar.Height := 28;
  FStatusBar.SimplePanel := False;
  FStatusBar.Panels.Add;
  FStatusBar.Panels.Add;
  FStatusBar.Panels.Add;
  FStatusBar.Panels[0].Text := '桥接地址：' + FBridgeUrl;
  FStatusBar.Panels[1].Text :=
    '配置目录：%USERPROFILE%\.claude\bridge-providers';
  FStatusBar.Font.Name := 'Segoe UI';

  FContentPanel := TPanel.Create(Self);
  FContentPanel.Parent := Self;
  FContentPanel.Align := alClient;
  FContentPanel.BevelOuter := bvNone;
  FContentPanel.Color := $00F5F5F5;
  FContentPanel.ParentBackground := False;
  FContentPanel.Padding.SetBounds(16, 14, 16, 14);

  FLogPanel := TPanel.Create(Self);
  FLogPanel.Parent := FContentPanel;
  FLogPanel.Align := alBottom;
  FLogPanel.AlignWithMargins := True;
  FLogPanel.Margins.SetBounds(0, 12, 0, 0);
  FLogPanel.Height := 126;
  FLogPanel.Constraints.MinHeight := FLogPanel.Height;
  FLogPanel.BevelOuter := bvNone;
  FLogPanel.Color := clWhite;
  FLogPanel.ParentBackground := False;
  FLogPanel.Padding.SetBounds(16, 40, 16, 12);

  FLogTitleLabel := TLabel.Create(Self);
  FLogTitleLabel.Parent := FLogPanel;
  FLogTitleLabel.SetBounds(16, 12, 300, 22);
  FLogTitleLabel.Caption := '运行记录';
  FLogTitleLabel.Font.Name := 'Segoe UI';
  FLogTitleLabel.Font.Style := [fsBold];
  FLogTitleLabel.Font.Color := $00303030;

  FLogMemo := TMemo.Create(Self);
  FLogMemo.Parent := FLogPanel;
  FLogMemo.Align := alClient;
  FLogMemo.BorderStyle := bsNone;
  FLogMemo.Color := $00FAFAFA;
  FLogMemo.ReadOnly := True;
  FLogMemo.ScrollBars := ssVertical;
  FLogMemo.Font.Name := 'Cascadia Mono';
  FLogMemo.Font.Size := 9;

  FLogSplitter := TSplitter.Create(Self);
  FLogSplitter.Parent := FContentPanel;
  FLogSplitter.Align := alBottom;
  FLogSplitter.Height := 5;
  FLogSplitter.Color := $00F5F5F5;
  FLogSplitter.Cursor := crSizeNS;
  FLogSplitter.MinSize := FLogPanel.Height;

  FListPanel := TPanel.Create(Self);
  FListPanel.Parent := FContentPanel;
  FListPanel.Align := alClient;
  FListPanel.BevelOuter := bvNone;
  FListPanel.Color := clWhite;
  FListPanel.ParentBackground := False;
  FListPanel.Padding.SetBounds(16, 52, 16, 12);

  FListTitleLabel := TLabel.Create(Self);
  FListTitleLabel.Parent := FListPanel;
  FListTitleLabel.SetBounds(16, 12, 220, 24);
  FListTitleLabel.Caption := '可用模型';
  FListTitleLabel.Font.Name := 'Segoe UI';
  FListTitleLabel.Font.Size := 11;
  FListTitleLabel.Font.Style := [fsBold];
  FListTitleLabel.Font.Color := $00242424;

  FListHintLabel := TLabel.Create(Self);
  FListHintLabel.Parent := FListPanel;
  FListHintLabel.SetBounds(146, 15, 340, 20);
  FListHintLabel.Caption := '双击模型即可切换，下一请求生效';
  FListHintLabel.Font.Name := 'Segoe UI';
  FListHintLabel.Font.Color := $00606060;

  FTestButton := TButton.Create(Self);
  FTestButton.Parent := FListPanel;
  FTestButton.SetBounds(844, 9, 112, 32);
  FTestButton.Caption := '测试选中模型';
  FTestButton.Enabled := False;
  FTestButton.OnClick := TestSelectedClick;

  FTestResultLabel := TLabel.Create(Self);
  FTestResultLabel.Parent := FListPanel;
  FTestResultLabel.SetBounds(500, 15, 230, 20);
  FTestResultLabel.Caption := '';
  FTestResultLabel.Font.Name := 'Segoe UI';
  FTestResultLabel.Font.Size := 10;
  FTestResultLabel.Font.Color := $00606060;
  FTestResultLabel.AutoSize := False;

  FProfileList := TListView.Create(Self);
  FProfileList.Parent := FListPanel;
  FProfileList.Align := alClient;
  FProfileList.ViewStyle := vsReport;
  FProfileList.BorderStyle := bsNone;
  FProfileList.ReadOnly := True;
  FProfileList.RowSelect := True;
  FProfileList.HideSelection := False;
  FProfileList.GridLines := False;
  FProfileList.OnDblClick := ProfileDblClick;
  FProfileList.OnSelectItem := ListSelectItem;
  FProfileList.Font.Name := 'Segoe UI';
  FProfileList.Font.Size := 10;

  LColumn := FProfileList.Columns.Add;
  LColumn.Caption := '模型';
  LColumn.Width := 205;
  LColumn := FProfileList.Columns.Add;
  LColumn.Caption := '服务地址';
  LColumn.Width := 285;
  LColumn := FProfileList.Columns.Add;
  LColumn.Caption := '代理';
  LColumn.Width := 140;
  LColumn := FProfileList.Columns.Add;
  LColumn.Caption := '路由';
  LColumn.Width := 105;
  LColumn := FProfileList.Columns.Add;
  LColumn.Caption := '配置文件';
  LColumn.Width := 165;

  FPollTimer := TTimer.Create(Self);
  FPollTimer.Interval := 3000;
  FPollTimer.Enabled := False;
  FPollTimer.OnTimer := PollTimer;
end;

procedure TMainForm.FormShown(Sender: TObject);
var
  LStage: string;
begin
  LStage := '记录桥接器目录';
  try
    FormResize(Self);
    ApplyWindowsAppearance;
    AppendLog('桥接器目录：' + FBridgeRoot);
    AppendLog('桥接地址：' + FBridgeUrl);
    LStage := '读取桥接器状态';
    RefreshStatusAsync(False);
    LStage := '启动状态轮询';
    FPollTimer.Enabled := True;
  except
    on E: Exception do
    begin
      MessageDlg(
        'GUI 初始化失败：' + LStage + sLineBreak + E.ClassName + ': ' + E.Message,
        mtError,
        [mbOK],
        0
      );
    end;
  end;
end;

procedure TMainForm.FormResize(Sender: TObject);
const
  CONTROL_GAP = 8;
  EDGE_MARGIN = 24;
begin
  if Assigned(FStopButton) and Assigned(FTopPanel) then
  begin
    FStopButton.Left :=
      FTopPanel.ClientWidth - EDGE_MARGIN - FStopButton.Width;
    FStartButton.Left :=
      FStopButton.Left - CONTROL_GAP - FStartButton.Width;
    FSwitchButton.Left :=
      FStartButton.Left - CONTROL_GAP - FSwitchButton.Width;
    FRefreshButton.Left :=
      FSwitchButton.Left - CONTROL_GAP - FRefreshButton.Width;
    FThinkingGroup.Left :=
      FTopPanel.ClientWidth - EDGE_MARGIN - FThinkingGroup.Width;
    FLsPanel.Left := FTopPanel.ClientWidth - EDGE_MARGIN - FLsPanel.Width;
    FProviderReasoningGroup.Left :=
      FTopPanel.ClientWidth - EDGE_MARGIN - FProviderReasoningGroup.Width;
  end;

  if Assigned(FStatusBar) then
  begin
    FStatusBar.Panels[0].Width := FStatusBar.ClientWidth * 2 div 5;
    FStatusBar.Panels[1].Width := FStatusBar.ClientWidth * 3 div 10;
  end;

  if Assigned(FTestButton) and Assigned(FTestResultLabel) and
     Assigned(FListPanel) then
  begin
    FTestButton.Left :=
      FListPanel.ClientWidth - 16 - FTestButton.Width;
    FTestResultLabel.Left :=
      FTestButton.Left - CONTROL_GAP - FTestResultLabel.Width;
  end;

  AdjustProfileColumns;
end;

procedure TMainForm.AdjustProfileColumns;
var
  LTotal: Integer;
  LModel: Integer;
  LUrl: Integer;
  LProxy: Integer;
  LRoute: Integer;
begin
  if not Assigned(FProfileList) or not FProfileList.HandleAllocated then
    Exit;
  LTotal := FProfileList.ClientWidth - GetSystemMetrics(SM_CXVSCROLL) - 4;
  if LTotal < 500 then
    Exit;
  LModel := Max(170, LTotal * 22 div 100);
  LUrl := Max(230, LTotal * 30 div 100);
  LProxy := Max(80, LTotal * 10 div 100);
  LRoute := Max(100, LTotal * 12 div 100);
  FProfileList.Columns[0].Width := LModel;
  FProfileList.Columns[1].Width := LUrl;
  FProfileList.Columns[2].Width := LProxy;
  FProfileList.Columns[3].Width := LRoute;
  FProfileList.Columns[4].Width :=
    Max(140, LTotal - LModel - LUrl - LProxy - LRoute);
end;

procedure TMainForm.ApplyWindowsAppearance;
var
  LDwmApi: HMODULE;
  LDwmSetWindowAttribute: TDwmSetWindowAttribute;
  LCornerPreference: Integer;
begin
  SetWindowTheme(FProfileList.Handle, 'Explorer', nil);
  SendMessage(
    FProfileList.Handle,
    LVM_SETEXTENDEDLISTVIEWSTYLE,
    LVS_EX_DOUBLEBUFFER,
    LVS_EX_DOUBLEBUFFER
  );

  LDwmApi := LoadLibrary('dwmapi.dll');
  if LDwmApi = 0 then
    Exit;
  try
    LDwmSetWindowAttribute := TDwmSetWindowAttribute(
      GetProcAddress(LDwmApi, PAnsiChar(AnsiString('DwmSetWindowAttribute')))
    );
    if Assigned(LDwmSetWindowAttribute) then
    begin
      LCornerPreference := DWMWCP_ROUND;
      LDwmSetWindowAttribute(
        Handle,
        DWMWA_WINDOW_CORNER_PREFERENCE,
        @LCornerPreference,
        SizeOf(LCornerPreference)
      );
    end;
  finally
    FreeLibrary(LDwmApi);
  end;
end;

procedure TMainForm.DoClose(var Action: TCloseAction);
begin
  FClosing := True;
  FPollTimer.Enabled := False;
  if FWorkerCount > 0 then
  begin
    Enabled := False;
    FStatusBar.Panels[0].Text := '正在等待后台操作结束...';
    Action := caNone;
    Exit;
  end;
  Action := caFree;
  Application.Terminate;
end;

procedure TMainForm.AppendLog(const AText: string);
begin
  FLogMemo.Lines.BeginUpdate;
  try
    FLogMemo.Lines.Add(FormatDateTime('hh:nn:ss', Now) + '  ' + AText);
    while FLogMemo.Lines.Count > LOG_MAX_LINES do
      FLogMemo.Lines.Delete(0);
    FLogMemo.SelStart := Length(FLogMemo.Text);
    FLogMemo.Perform(EM_SCROLLCARET, 0, 0);
  finally
    FLogMemo.Lines.EndUpdate;
  end;
end;

procedure TMainForm.QueueToMain(const AProc: TProc);
var
  LQueued: TThreadProcedure;
begin
  LQueued :=
    procedure
    begin
      try
        if not FClosing then
          AProc();
      finally
        if FWorkerCount > 0 then
          Dec(FWorkerCount);
        if FClosing and (FWorkerCount = 0) and HandleAllocated then
          PostMessage(Handle, WM_CLOSE, 0, 0);
      end;
    end;
  TThread.Queue(nil, LQueued);
end;

function TMainForm.NewHttpClient(const AResponseTimeout: Integer): THTTPClient;
begin
  Result := THTTPClient.Create;
  Result.ConnectionTimeout := 2000;
  Result.ResponseTimeout := AResponseTimeout;
end;

function TMainForm.JsonText(AObject: TJSONObject; const AName: string): string;
var
  LValue: TJSONValue;
begin
  Result := '';
  if not Assigned(AObject) then
    Exit;
  LValue := AObject.Values[AName];
  if not Assigned(LValue) or (LValue is TJSONNull) then
    Exit;
  Result := LValue.Value;
end;

function TMainForm.JsonBool(AObject: TJSONObject; const AName: string): Boolean;
begin
  Result := SameText(JsonText(AObject, AName), 'true');
end;

// 字符串数组 → 逗号连接的纯文本（如 status 的 choices），供 TStringList
// DelimitedText 直接解析；空数组返回空串。
function TMainForm.JsonArrayText(AObject: TJSONObject; const AName: string): string;
var
  LArray: TJSONValue;
  LItem: TJSONValue;
  I: Integer;
begin
  Result := '';
  if not Assigned(AObject) then
    Exit;
  LArray := AObject.Values[AName];
  if not (LArray is TJSONArray) then
    Exit;
  for I := 0 to TJSONArray(LArray).Count - 1 do
  begin
    LItem := TJSONArray(LArray).Items[I];
    if not (LItem is TJSONString) then
      Continue;
    if Result <> '' then
      Result := Result + ',';
    Result := Result + TJSONString(LItem).Value;
  end;
end;

function TMainForm.GetJsonWith(AClient: THTTPClient; const APath: string): TJSONObject;
var
  LHeaders: TNetHeaders;
  LResponse: IHTTPResponse;
  LValue: TJSONValue;
begin
  SetLength(LHeaders, 1);
  LHeaders[0].Name := 'Authorization';
  LHeaders[0].Value := 'Bearer ' + FLocalAuthToken;
  LResponse := AClient.Get(FBridgeUrl + APath, nil, LHeaders);
  if LResponse.StatusCode <> 200 then
    raise Exception.CreateFmt('HTTP %d: %s', [
      LResponse.StatusCode,
      LResponse.ContentAsString(TEncoding.UTF8)
    ]);
  LValue := TJSONObject.ParseJSONValue(
    LResponse.ContentAsString(TEncoding.UTF8)
  );
  if not (LValue is TJSONObject) then
  begin
    LValue.Free;
    raise Exception.Create('桥接器返回了无效 JSON');
  end;
  Result := TJSONObject(LValue);
end;

function TMainForm.PostJsonWith(AClient: THTTPClient; const APath,
  AJson: string): TJSONObject;
var
  LHeaders: TNetHeaders;
  LResponse: IHTTPResponse;
  LStream: TStringStream;
  LValue: TJSONValue;
begin
  SetLength(LHeaders, 2);
  LHeaders[0].Name := 'Content-Type';
  LHeaders[0].Value := 'application/json';
  LHeaders[1].Name := 'Authorization';
  LHeaders[1].Value := 'Bearer ' + FLocalAuthToken;
  LStream := TStringStream.Create(AJson, TEncoding.UTF8);
  try
    LResponse := AClient.Post(
      FBridgeUrl + APath,
      LStream,
      nil,
      LHeaders
    );
  finally
    LStream.Free;
  end;
  if LResponse.StatusCode <> 200 then
    raise Exception.CreateFmt('HTTP %d: %s', [
      LResponse.StatusCode,
      LResponse.ContentAsString(TEncoding.UTF8)
    ]);
  LValue := TJSONObject.ParseJSONValue(
    LResponse.ContentAsString(TEncoding.UTF8)
  );
  if not (LValue is TJSONObject) then
  begin
    LValue.Free;
    raise Exception.Create('桥接器返回了无效 JSON');
  end;
  Result := TJSONObject(LValue);
end;

function TMainForm.ReadStateListen(const AStatePath: string): string;
var
  LValue: TJSONValue;
begin
  Result := '';
  if (AStatePath = '') or not FileExists(AStatePath) then
    Exit;
  LValue := TJSONObject.ParseJSONValue(TFile.ReadAllText(AStatePath));
  if LValue is TJSONObject then
    Result := JsonText(TJSONObject(LValue), 'listen');
  LValue.Free;
  if Result <> '' then
    Result := 'http://' + Result;
end;

function TMainForm.ReadServiceStateFilePath: string;
var
  LKey: HKEY;
  LValueType: DWORD;
  LSize: DWORD;
  LBuffer: TBytes;
  LText: string;
  LPart: string;
begin
  Result := '';
  if RegOpenKeyEx(
    HKEY_LOCAL_MACHINE,
    'SYSTEM\CurrentControlSet\Services\ClaudeCodeBridge',
    0,
    KEY_READ,
    LKey
  ) <> ERROR_SUCCESS then
    Exit;
  try
    LSize := 0;
    if RegQueryValueEx(LKey, 'Environment', nil, @LValueType, nil, @LSize) <>
      ERROR_SUCCESS then
      Exit;
    if (LValueType <> REG_MULTI_SZ) or (LSize = 0) then
      Exit;
    SetLength(LBuffer, LSize);
    if RegQueryValueEx(LKey, 'Environment', nil, @LValueType, @LBuffer[0],
      @LSize) <> ERROR_SUCCESS then
      Exit;
    LText := TEncoding.Unicode.GetString(LBuffer);
    for LPart in LText.Split([#0]) do
      if LPart.StartsWith('GEMINI_BRIDGE_STATE_FILE=') then
        Exit(LPart.Substring(Length('GEMINI_BRIDGE_STATE_FILE=')));
  finally
    RegCloseKey(LKey);
  end;
end;

function TMainForm.ReadLocalAuthToken: string;
begin
  if not FileExists(LocalAuthTokenPath) then
    raise Exception.Create('找不到桥接器本地认证令牌，请重新运行安装程序');
  Result := Trim(TFile.ReadAllText(LocalAuthTokenPath, TEncoding.UTF8));
  if Length(Result) < 32 then
    raise Exception.Create('桥接器本地认证令牌无效，请重新运行安装程序');
end;

function TMainForm.LocalAuthTokenPath: string;
var
  LProgramData: string;
begin
  LProgramData := GetEnvironmentVariable('ProgramData');
  if LProgramData = '' then
    raise Exception.Create('无法定位 ProgramData，不能读取桥接器本地认证令牌');
  Result := TPath.Combine(
    TPath.Combine(LProgramData, 'ClaudeCodeBridge'),
    'local-auth-token'
  );
end;

function TMainForm.ResolveBridgeUrl: string;
var
  LStatePath: string;
  LListenUrl: string;
begin
  Result := DEFAULT_BRIDGE_URL;
  try
    LStatePath := GetEnvironmentVariable('GEMINI_BRIDGE_STATE_FILE');
    if LStatePath = '' then
      LStatePath := FBridgeRoot + '\bridge-state.json';
    LListenUrl := ReadStateListen(LStatePath);
    if (LListenUrl = '') and
       (not SameText(LStatePath, FBridgeRoot + '\bridge-state.json')) then
      Exit;
    if LListenUrl = '' then
      LListenUrl := ReadStateListen(ReadServiceStateFilePath);
    if LListenUrl <> '' then
      Result := LListenUrl;
  except
    Result := DEFAULT_BRIDGE_URL;
  end;
end;

procedure TMainForm.SetBusy(const ABusy: Boolean);
begin
  FBusy := ABusy;
  if ABusy then
  begin
    FRefreshButton.Enabled := False;
    FProfileList.Enabled := False;
    FSwitchButton.Enabled := False;
    FTestButton.Enabled := False;
    FStartButton.Enabled := False;
    FStopButton.Enabled := False;
    FThinkingGroup.Enabled := False;
    FProviderReasoningGroup.Enabled := False;
    Screen.Cursor := crHourGlass;
  end
  else
  begin
    Screen.Cursor := crDefault;
    FRefreshButton.Enabled := True;
    FProfileList.Enabled := True;
    // 探测在途时保持“测试”按钮禁用，防止重复探测。
    FTestButton.Enabled := (FStatusLabel.Tag = 1) and not FTestInFlight;
    FThinkingGroup.Enabled :=
      (FStatusLabel.Tag = 1) and (FThinkingLevel <> '') and not FThinkingLocked;
    FProviderReasoningGroup.Enabled :=
      (FStatusLabel.Tag = 1) and FProviderReasoningSupported and
      (FProviderReasoningCurrent <> '');
    RefreshStatusAsync(True);
  end;
end;

procedure TMainForm.SetBridgeState(
  const AOnline: Boolean;
  const ADetail: string
);
begin
  if AOnline then
  begin
    FStatusLabel.Caption := '桥接器：运行中';
    FStatusLabel.Font.Color := $00106410;
    FStatusDotLabel.Font.Color := $00107C10;
    FStatusPanel.Color := $00ECF6E9;
    FStatusLabel.Tag := 1;
    FSwitchButton.Enabled := True;
    // 探测在途时“测试”按钮保持禁用。
    FTestButton.Enabled := not FTestInFlight;
    FStopButton.Enabled := True;
    FStartButton.Enabled := False;
  end
  else
  begin
    FStatusLabel.Caption := '桥接器：未运行';
    FStatusLabel.Font.Color := $001C2BC4;
    FStatusDotLabel.Font.Color := $001C2BC4;
    FStatusPanel.Color := $00EDEFFB;
    FStatusLabel.Tag := 0;
    FCurrentModelLabel.Caption := '当前模型：-';
    FThinkingGroup.Enabled := False;
    FProviderReasoningGroup.Enabled := False;
    FSwitchButton.Enabled := False;
    FTestButton.Enabled := False;
    FStopButton.Enabled := False;
    FStartButton.Enabled := True;
  end;
  if FBusy then
  begin
    FRefreshButton.Enabled := False;
    FSwitchButton.Enabled := False;
    FTestButton.Enabled := False;
    FStartButton.Enabled := False;
    FStopButton.Enabled := False;
  end;
  FStatusBar.Panels[0].Text := ADetail;
end;

procedure TMainForm.RefreshStatusAsync(const AQuiet: Boolean);
begin
  if FClosing or FStatusInFlight then
    Exit;
  FStatusInFlight := True;
  Inc(FWorkerCount);
  TThread.CreateAnonymousThread(
    procedure
    var
      LClient: THTTPClient;
      LJson: TJSONObject;
      LProfile: TJSONObject;
      LControl: TJSONObject;
      LSnapshot: TStatusSnapshot;
    begin
      LSnapshot := Default(TStatusSnapshot);
      try
        LClient := NewHttpClient(STATUS_RESPONSE_TIMEOUT_MS);
        try
          LJson := GetJsonWith(LClient, '/admin/status');
          try
            LSnapshot.Online := True;
            if LJson.Values['active_profile'] is TJSONObject then
            begin
              LProfile := TJSONObject(LJson.Values['active_profile']);
              LSnapshot.HasProfile := True;
              LSnapshot.Model := JsonText(LProfile, 'model');
              LSnapshot.FileName := JsonText(LProfile, 'file');
              LSnapshot.Transport := JsonText(LProfile, 'transport');
            end;
            if LJson.Values['ls_host'] is TJSONObject then
            begin
              LControl := TJSONObject(LJson.Values['ls_host']);
              LSnapshot.LsControls := LControl.ToJSON;
              LSnapshot.Detail := 'LS Host: ' + JsonText(LControl, 'status');
              if JsonText(LControl, 'engine_version') <> '' then
                LSnapshot.Detail := LSnapshot.Detail + ' / ' +
                  JsonText(LControl, 'engine_version');
            end;
            LSnapshot.ThinkingLevel := JsonText(
              LJson,
              'gemini_thinking_level'
            );
            LSnapshot.ThinkingLocked := JsonBool(LJson, 'gemini_thinking_locked');
            LSnapshot.ThinkingChoices := JsonArrayText(LJson, 'gemini_thinking_choices');
            if LJson.Values['reasoning_effort_control'] is TJSONObject then
            begin
              LControl := TJSONObject(LJson.Values['reasoning_effort_control']);
              LSnapshot.ProviderReasoningSupported :=
                JsonBool(LControl, 'supported');
              LSnapshot.ProviderReasoningChoices :=
                JsonArrayText(LControl, 'choices');
              LSnapshot.ProviderReasoningCurrent := JsonText(LControl, 'current');
            end;
            LSnapshot.Stamp := JsonText(LJson, 'config_stamp');
            if LSnapshot.Stamp = '' then
              LSnapshot.Stamp := JsonText(LJson, 'settings_stamp');
            LSnapshot.SettingsDir := JsonText(LJson, 'providers_dir');
            if LSnapshot.SettingsDir = '' then
              LSnapshot.SettingsDir := JsonText(LJson, 'settings_dir');
          finally
            LJson.Free;
          end;
        finally
          LClient.Free;
        end;
      except
        on E: Exception do
        begin
          LSnapshot.Online := False;
          LSnapshot.Detail := E.Message;
        end;
      end;
      QueueToMain(
        procedure
        begin
          FStatusInFlight := False;
          ApplyStatusSnapshot(LSnapshot, AQuiet);
        end);
    end).Start;
end;

procedure TMainForm.ApplyStatusSnapshot(
  const ASnapshot: TStatusSnapshot;
  const AQuiet: Boolean);
var
  LStampChanged: Boolean;
begin
  if ASnapshot.Online then
  begin
    if ASnapshot.HasProfile then
    begin
      FActiveFile := ASnapshot.FileName;
      FCurrentModelLabel.Caption := '当前模型：' + ASnapshot.Model;
      if ASnapshot.Detail <> '' then
        SetBridgeState(True, '当前配置：' + ASnapshot.FileName + ' | ' + ASnapshot.Detail)
      else
        SetBridgeState(True, '当前配置：' + ASnapshot.FileName);
    end
    else
    begin
      FActiveFile := '';
      FCurrentModelLabel.Caption := '当前模型：暂无可用配置';
      SetBridgeState(
        True,
        '桥接器运行中，请在 bridge-providers 目录添加 JSON 配置'
      );
      FSwitchButton.Enabled := False;
    end;
    if ASnapshot.SettingsDir <> '' then
      FStatusBar.Panels[1].Text := '配置目录：' + ASnapshot.SettingsDir;
    ApplyThinkingControls(ASnapshot);
    ApplyProviderReasoningControls(ASnapshot);
    ApplyLsControls(ASnapshot);
    LStampChanged :=
      (ASnapshot.Stamp <> '') and (ASnapshot.Stamp <> FSettingsStamp);
    if LStampChanged then
    begin
      FSettingsStamp := ASnapshot.Stamp;
      StartProfilesRefresh(FProfilesLoaded);
    end;
  end
  else
  begin
    SetBridgeState(False, ASnapshot.Detail);
    ApplyThinkingControls(ASnapshot);
    ApplyProviderReasoningControls(ASnapshot);
    ApplyLsControls(ASnapshot);
    if not AQuiet then
      AppendLog('状态检查失败：' + ASnapshot.Detail);
  end;
end;

procedure TMainForm.ApplyThinkingControls(const ASnapshot: TStatusSnapshot);
var
  LSupported: Boolean;
begin
  LSupported := ASnapshot.Online and ASnapshot.HasProfile and
    SameText(ASnapshot.Transport, 'gemini-interactions') and
    (ASnapshot.ThinkingLevel <> '');
  FUpdatingThinkingUi := True;
  try
    FThinkingLocked := ASnapshot.ThinkingLocked;
    if LSupported then
    begin
      FThinkingGroup.Caption := 'Gemini Thinking（下一请求生效）';
      // The service owns the model capability list; older services retain three tiers.
      if Pos('xhigh', ASnapshot.ThinkingChoices) > 0 then
      begin
        if FThinkingGroup.Items.Count = 3 then
          FThinkingGroup.Items.Add('极高');
        FThinkingGroup.Columns := 4;
      end
      else
      begin
        if FThinkingGroup.Items.Count > 3 then
          FThinkingGroup.Items.Delete(3);
        FThinkingGroup.Columns := 3;
      end;
      FThinkingLevel := LowerCase(ASnapshot.ThinkingLevel);
      if SameText(FThinkingLevel, 'low') then
        FThinkingGroup.ItemIndex := 0
      else if SameText(FThinkingLevel, 'medium') then
        FThinkingGroup.ItemIndex := 1
      else if SameText(FThinkingLevel, 'high') then
        FThinkingGroup.ItemIndex := 2
      else if SameText(FThinkingLevel, 'xhigh') and (FThinkingGroup.Items.Count = 4) then
        FThinkingGroup.ItemIndex := 3
      else
        FThinkingGroup.ItemIndex := -1;
      FThinkingGroup.Enabled := not FBusy and not FThinkingLocked;
      if FThinkingLocked then
        FThinkingGroup.Caption := 'Gemini Thinking（配置已锁定）';
    end
    else
    begin
      FThinkingLevel := '';
      FThinkingGroup.ItemIndex := -1;
      FThinkingGroup.Enabled := False;
      FThinkingGroup.Caption := 'Gemini Thinking（当前模型不支持调档）';
    end;
  finally
    FUpdatingThinkingUi := False;
  end;
end;

procedure TMainForm.ApplyProviderReasoningControls(const ASnapshot: TStatusSnapshot);
var
  LChoices: TStringList;
  LVisible: Boolean;
  LIndex: Integer;
begin
  LChoices := TStringList.Create;
  try
    LChoices.DelimitedText := ASnapshot.ProviderReasoningChoices;
    // 支持且 choices 有效时显示四档组；current 缺失（无 runtime 值也没有
    // profile 静态默认）时不选择任何 radio 并保持禁用，不凭空声称档位。
    LVisible := ASnapshot.Online and ASnapshot.HasProfile and
      ASnapshot.ProviderReasoningSupported and (LChoices.Count > 0);
    FUpdatingProviderReasoningUi := True;
    try
      if LVisible then
      begin
        FProviderReasoningGroup.Caption := 'Provider Reasoning（下一请求生效）';
        // 只在 choices 实际变化时重建选项，避免 3 秒轮询反复重建按钮。
        if FProviderReasoningChoices.CommaText <> LChoices.CommaText then
        begin
          FProviderReasoningGroup.Items.Assign(LChoices);
          FProviderReasoningChoices.Assign(LChoices);
        end;
        FProviderReasoningSupported := True;
        FProviderReasoningCurrent :=
          LowerCase(ASnapshot.ProviderReasoningCurrent);
        LIndex := FProviderReasoningChoices.IndexOf(FProviderReasoningCurrent);
        FProviderReasoningGroup.ItemIndex := LIndex;
        FProviderReasoningGroup.Visible := True;
        FProviderReasoningGroup.Enabled := (LIndex >= 0) and not FBusy;
        // Provider Reasoning 显示在上层时，Gemini 组同时隐藏且禁用，两个
        // 控件任何时刻都不会同时可操作。
        FThinkingGroup.Visible := False;
        FThinkingGroup.Enabled := False;
      end
      else
      begin
        FProviderReasoningSupported := False;
        FProviderReasoningCurrent := '';
        FProviderReasoningChoices.Clear;
        FProviderReasoningGroup.Items.Clear;
        FProviderReasoningGroup.ItemIndex := -1;
        FProviderReasoningGroup.Enabled := False;
        FProviderReasoningGroup.Visible := False;
        FThinkingGroup.Visible := not SameText(ASnapshot.Transport, 'antigravity-ls');
      end;
    finally
      FUpdatingProviderReasoningUi := False;
    end;
  finally
    LChoices.Free;
  end;
end;

procedure TMainForm.StartProfilesRefresh(const AQuiet: Boolean);
begin
  if FClosing or FProfilesBusy then
    Exit;
  FProfilesBusy := True;
  if not AQuiet then
    SetBusy(True);
  Inc(FWorkerCount);
  TThread.CreateAnonymousThread(
    procedure
    var
      LClient: THTTPClient;
      LReload: TJSONObject;
      LJson: TJSONObject;
      LArray: TJSONArray;
      LProfile: TJSONObject;
      LRows: TArray<TProfileRow>;
      LRow: TProfileRow;
      LStamp: string;
      LTransport: string;
      LName: string;
      LError: string;
      I: Integer;
    begin
      LError := '';
      try
        LClient := NewHttpClient(ACTION_RESPONSE_TIMEOUT_MS);
        try
          LReload := PostJsonWith(LClient, '/admin/reload-profiles', '{}');
          LReload.Free;
          LJson := GetJsonWith(LClient, '/admin/profiles');
          try
            LArray := LJson.Values['profiles'] as TJSONArray;
            SetLength(LRows, LArray.Count);
            for I := 0 to LArray.Count - 1 do
            begin
              LProfile := LArray.Items[I] as TJSONObject;
              LRow := Default(TProfileRow);
              LRow.Model := JsonText(LProfile, 'model');
              LName := JsonText(LProfile, 'name');
              if (LName <> '') and not SameText(LName, LRow.Model) then
                LRow.Model := LName + ' (' + LRow.Model + ')';
              LRow.FileName := JsonText(LProfile, 'file');
              LRow.BaseUrl := JsonText(LProfile, 'base_url');
              LRow.Proxy := JsonText(LProfile, 'proxy');
              LRow.Active := JsonBool(LProfile, 'active');
              LTransport := JsonText(LProfile, 'transport');
              if SameText(LTransport, 'antigravity-ls') then
                LRow.Route := 'LS 自主执行'
              else if SameText(LTransport, 'gemini-interactions') then
                LRow.Route := 'Gemini 原生'
              else if SameText(LTransport, 'gemini') or
                 JsonBool(LProfile, 'local_gemini') then
                LRow.Route := 'Gemini 转换'
              else if SameText(LTransport, 'openai-chat') then
                LRow.Route := 'OpenAI 转换'
              else
                LRow.Route := 'Anthropic 直通';
              if LRow.Proxy = '' then
                LRow.Proxy := '(直连)';
              LRows[I] := LRow;
            end;
            LStamp := JsonText(LJson, 'config_stamp');
            if LStamp = '' then
              LStamp := JsonText(LJson, 'settings_stamp');
          finally
            LJson.Free;
          end;
        finally
          LClient.Free;
        end;
      except
        on E: Exception do
          LError := E.Message;
      end;
      QueueToMain(
        procedure
        begin
          FProfilesBusy := False;
          if not AQuiet then
            SetBusy(False);
          if LError <> '' then
          begin
            AppendLog('刷新失败：' + LError);
            if not AQuiet then
              MessageDlg(LError, mtError, [mbOK], 0);
          end
          else
          begin
            FillProfileList(LRows);
            FProfilesLoaded := True;
            FSettingsStamp := LStamp;
            AppendLog(Format('已加载 %d 个模型配置。', [Length(LRows)]));
          end;
        end);
    end).Start;
end;

procedure TMainForm.FillProfileList(const ARows: TArray<TProfileRow>);
var
  I: Integer;
  LItem: TListItem;
  LRow: TProfileRow;
begin
  FProfileList.Items.BeginUpdate;
  try
    FProfileList.Items.Clear;
    FProfileFiles.Clear;
    for I := 0 to High(ARows) do
    begin
      LRow := ARows[I];
      LItem := FProfileList.Items.Add;
      if LRow.Active then
      begin
        LItem.Caption := '✓  ' + LRow.Model;
        LItem.Selected := True;
        LItem.Focused := True;
        FActiveFile := LRow.FileName;
      end
      else
        LItem.Caption := LRow.Model;
      LItem.SubItems.Add(LRow.BaseUrl);
      LItem.SubItems.Add(LRow.Proxy);
      LItem.SubItems.Add(LRow.Route);
      LItem.SubItems.Add(LRow.FileName);
      FProfileFiles.Add(LRow.FileName);
    end;
  finally
    FProfileList.Items.EndUpdate;
  end;
  AdjustProfileColumns;
end;

procedure TMainForm.ListSelectItem(Sender: TObject; Item: TListItem;
  Selected: Boolean);
begin
  if Selected and (Item.SubItems.Count >= 4) then
    FStatusBar.Panels[2].Text := '选中：' + Item.SubItems[3];
end;

procedure TMainForm.SwitchProfileAsync(const AFileName: string);
begin
  if FClosing or FBusy then
    Exit;
  SetBusy(True);
  Inc(FWorkerCount);
  TThread.CreateAnonymousThread(
    procedure
    var
      LClient: THTTPClient;
      LRequest: TJSONObject;
      LResult: TJSONObject;
      LError: string;
    begin
      LError := '';
      try
        LClient := NewHttpClient(ACTION_RESPONSE_TIMEOUT_MS);
        try
          LRequest := TJSONObject.Create;
          try
            LRequest.AddPair('file', AFileName);
            LResult := PostJsonWith(
              LClient,
              '/admin/active-profile',
              LRequest.ToJSON
            );
          finally
            LRequest.Free;
          end;
          LResult.Free;
        finally
          LClient.Free;
        end;
      except
        on E: Exception do
          LError := E.Message;
      end;
      QueueToMain(
        procedure
        begin
          SetBusy(False);
          if LError <> '' then
          begin
            AppendLog('切换失败：' + LError);
            MessageDlg(LError, mtError, [mbOK], 0);
          end
          else
          begin
            AppendLog('模型已切换：' + AFileName);
            StartProfilesRefresh(True);
          end;
        end);
    end).Start;
end;

procedure TMainForm.ApplyLsControls(const ASnapshot: TStatusSnapshot);
var
  Root, Group, Variant: TJSONObject;
  Groups, Variants: TJSONArray;
  I, J, Selected: Integer;
begin
  FLsPanel.Visible := ASnapshot.Online and SameText(ASnapshot.Transport, 'antigravity-ls');
  if not FLsPanel.Visible then Exit;
  FThinkingGroup.Visible := False;
  FProviderReasoningGroup.Visible := False;
  if FBusy or FLsModel.DroppedDown or FLsReasoning.DroppedDown then Exit;
  if (FLastLsControls <> '') and (FLastLsControls = ASnapshot.LsControls) and
    (FLsCurrentModel = ASnapshot.Model) then Exit;
  FUpdatingLs := True;
  Root := TJSONObject.ParseJSONValue(ASnapshot.LsControls) as TJSONObject;
  FLsModel.Items.BeginUpdate;
  FLsReasoning.Items.BeginUpdate;
  try
    FLsModel.Items.Clear;
    FLsReasoning.Items.Clear;
    FLsGroups := '';
    FLsCurrentModel := ASnapshot.Model;
    FLsModel.Enabled := False;
    FLsReasoning.Enabled := False;
    if (Root = nil) or not (Root.Values['groups'] is TJSONArray) then Exit;
    Groups := TJSONArray(Root.Values['groups']);
    FLsGroups := Groups.ToJSON;
    Selected := -1;
    for I := 0 to Groups.Count - 1 do
    begin
      Group := Groups.Items[I] as TJSONObject;
      FLsModel.Items.Add(JsonText(Group, 'label'));
      Variants := Group.Values['variants'] as TJSONArray;
      for J := 0 to Variants.Count - 1 do
        if JsonText(Variants.Items[J] as TJSONObject, 'id') = ASnapshot.Model then Selected := I;
    end;
    FLsModel.ItemIndex := Selected;
    FLsModel.Enabled := Groups.Count > 0;
    if Selected < 0 then Exit;
    Variants := (Groups.Items[Selected] as TJSONObject).Values['variants'] as TJSONArray;
    for J := 0 to Variants.Count - 1 do
    begin
      Variant := Variants.Items[J] as TJSONObject;
      FLsReasoning.Items.Add(JsonText(Variant, 'level'));
      if JsonText(Variant, 'id') = ASnapshot.Model then FLsReasoning.ItemIndex := J;
    end;
    FLsReasoning.Enabled := Variants.Count > 1;
  finally
    FLastLsControls := ASnapshot.LsControls;
    FLsReasoning.Items.EndUpdate;
    FLsModel.Items.EndUpdate;
    Root.Free;
    FUpdatingLs := False;
  end;
end;

procedure TMainForm.LsModelChange(Sender: TObject);
var
  Groups, Variants: TJSONArray;
  I: Integer;
  Chosen, Level: string;
begin
  if FUpdatingLs or FBusy or (FLsModel.ItemIndex < 0) then Exit;
  Level := FLsReasoning.Text;
  Groups := TJSONObject.ParseJSONValue(FLsGroups) as TJSONArray;
  try
    Variants := (Groups.Items[FLsModel.ItemIndex] as TJSONObject).Values['variants'] as TJSONArray;
    Chosen := JsonText(Variants.Items[0] as TJSONObject, 'id');
    for I := 0 to Variants.Count - 1 do
      if JsonText(Variants.Items[I] as TJSONObject, 'level') = Level then
        Chosen := JsonText(Variants.Items[I] as TJSONObject, 'id');
    SetLsModelAsync(Chosen);
  finally
    Groups.Free;
  end;
end;

procedure TMainForm.LsReasoningChange(Sender: TObject);
var
  Groups, Variants: TJSONArray;
begin
  if FUpdatingLs or FBusy or (FLsModel.ItemIndex < 0) or (FLsReasoning.ItemIndex < 0) then Exit;
  Groups := TJSONObject.ParseJSONValue(FLsGroups) as TJSONArray;
  try
    Variants := (Groups.Items[FLsModel.ItemIndex] as TJSONObject).Values['variants'] as TJSONArray;
    SetLsModelAsync(JsonText(Variants.Items[FLsReasoning.ItemIndex] as TJSONObject, 'id'));
  finally
    Groups.Free;
  end;
end;

procedure TMainForm.SetLsModelAsync(const AModel: string);
var
  Profile, Previous: string;
begin
  if FClosing or FBusy or (AModel = FLsCurrentModel) then Exit;
  Profile := FActiveFile;
  Previous := FLsCurrentModel;
  FLastLsControls := ''; // Also rebuild after a rejected model change.
  SetBusy(True);
  FLsModel.Enabled := False;
  FLsReasoning.Enabled := False;
  Inc(FWorkerCount);
  TThread.CreateAnonymousThread(
    procedure
    var
      Client: THTTPClient;
      Request, Response: TJSONObject;
      ErrorText: string;
    begin
      ErrorText := '';
      try
        Client := NewHttpClient(ACTION_RESPONSE_TIMEOUT_MS);
        try
          Request := TJSONObject.Create;
          try
            Request.AddPair('profile', Profile);
            Request.AddPair('previous_model', Previous);
            Request.AddPair('model', AModel);
            Response := PostJsonWith(Client, '/admin/ls/model', Request.ToJSON);
            Response.Free;
          finally
            Request.Free;
          end;
        finally
          Client.Free;
        end;
      except
        on E: Exception do ErrorText := E.Message;
      end;
      QueueToMain(
        procedure
        begin
          SetBusy(False);
          if ErrorText <> '' then AppendLog('LS 模型切换失败：' + ErrorText)
          else AppendLog('LS 模型已保存：' + AModel + '；请开始新会话');
          RefreshStatusAsync(True);
          StartProfilesRefresh(True);
        end);
    end).Start;
end;

procedure TMainForm.RefreshClick(Sender: TObject);
begin
  StartProfilesRefresh(False);
end;

procedure TMainForm.SetThinkingLevelAsync(const ALevel: string);
begin
  if FClosing or FBusy then
    Exit;
  SetBusy(True);
  Inc(FWorkerCount);
  TThread.CreateAnonymousThread(
    procedure
    var
      LClient: THTTPClient;
      LRequest: TJSONObject;
      LResult: TJSONObject;
      LAppliedLevel: string;
      LError: string;
    begin
      LAppliedLevel := '';
      LError := '';
      try
        LClient := NewHttpClient(ACTION_RESPONSE_TIMEOUT_MS);
        try
          LRequest := TJSONObject.Create;
          try
            LRequest.AddPair('level', ALevel);
            LResult := PostJsonWith(
              LClient,
              '/admin/gemini-thinking-level',
              LRequest.ToJSON
            );
          finally
            LRequest.Free;
          end;
          try
            LAppliedLevel := JsonText(LResult, 'gemini_thinking_level');
          finally
            LResult.Free;
          end;
        finally
          LClient.Free;
        end;
      except
        on E: Exception do
          LError := E.Message;
      end;
      QueueToMain(
        procedure
        begin
          SetBusy(False);
          if LError <> '' then
          begin
            AppendLog('Thinking 档位切换失败：' + LError);
            MessageDlg(LError, mtError, [mbOK], 0);
          end
          else
          begin
            FThinkingLevel := LAppliedLevel;
            AppendLog('Gemini Thinking 已切换为：' + LAppliedLevel);
          end;
          RefreshStatusAsync(True);
        end);
    end).Start;
end;

procedure TMainForm.ThinkingLevelClick(Sender: TObject);
var
  LLevel: string;
begin
  if FUpdatingThinkingUi or FBusy or not FThinkingGroup.Enabled then
    Exit;
  case FThinkingGroup.ItemIndex of
    0: LLevel := 'low';
    1: LLevel := 'medium';
    2: LLevel := 'high';
    3: LLevel := 'xhigh';
  else
    Exit;
  end;
  if SameText(LLevel, FThinkingLevel) then
    Exit;
  SetThinkingLevelAsync(LLevel);
end;

procedure TMainForm.SetProviderReasoningAsync(const ALevel: string);
begin
  if FClosing or FBusy then
    Exit;
  SetBusy(True);
  Inc(FWorkerCount);
  TThread.CreateAnonymousThread(
    procedure
    var
      LClient: THTTPClient;
      LRequest: TJSONObject;
      LResult: TJSONObject;
      LError: string;
    begin
      LError := '';
      try
        LClient := NewHttpClient(ACTION_RESPONSE_TIMEOUT_MS);
        try
          LRequest := TJSONObject.Create;
          try
            LRequest.AddPair('level', ALevel);
            LResult := PostJsonWith(
              LClient,
              '/admin/reasoning-effort',
              LRequest.ToJSON
            );
          finally
            LRequest.Free;
          end;
          LResult.Free;
        finally
          LClient.Free;
        end;
      except
        on E: Exception do
          LError := E.Message;
      end;
      QueueToMain(
        procedure
        begin
          SetBusy(False);
          if LError <> '' then
          begin
            // 不在本地留下虚假选择：下一次状态刷新恢复服务端真实 current。
            AppendLog('Provider Reasoning 档位切换失败：' + LError);
            MessageDlg(LError, mtError, [mbOK], 0);
          end
          else
            AppendLog('Provider Reasoning 已切换为：' + ALevel);
          RefreshStatusAsync(True);
        end);
    end).Start;
end;

procedure TMainForm.ProviderReasoningClick(Sender: TObject);
var
  LLevel: string;
begin
  if FUpdatingProviderReasoningUi or FBusy or
     not FProviderReasoningGroup.Enabled then
    Exit;
  // 按当前 choices 数组的 ItemIndex 取实际字符串：Qwen 与 DeepSeek 的
  // 第三、四项内容不同，不能把索引硬编码成两个模型共用的档位名。
  if (FProviderReasoningGroup.ItemIndex < 0) or
     (FProviderReasoningGroup.ItemIndex >= FProviderReasoningChoices.Count) then
    Exit;
  LLevel := FProviderReasoningChoices[FProviderReasoningGroup.ItemIndex];
  if SameText(LLevel, FProviderReasoningCurrent) then
    Exit;
  SetProviderReasoningAsync(LLevel);
end;

procedure TMainForm.SwitchClick(Sender: TObject);
begin
  if not Assigned(FProfileList.Selected) then
  begin
    MessageDlg('请先选择一个模型配置。', mtInformation, [mbOK], 0);
    Exit;
  end;
  if (FProfileList.Selected.Index < 0) or
     (FProfileList.Selected.Index >= FProfileFiles.Count) then
    Exit;
  SwitchProfileAsync(FProfileFiles[FProfileList.Selected.Index]);
end;

procedure TMainForm.TestSelectedClick(Sender: TObject);
begin
  if not Assigned(FProfileList.Selected) then
  begin
    MessageDlg('请先选择一个模型配置。', mtInformation, [mbOK], 0);
    Exit;
  end;
  if (FProfileList.Selected.Index < 0) or
     (FProfileList.Selected.Index >= FProfileFiles.Count) then
    Exit;
  TestProfileAsync(FProfileFiles[FProfileList.Selected.Index]);
end;

// 探测失败阶段：探针 API 返回的稳定阶段标识 → 简洁中文标签。
function TMainForm.ProbeStageName(const AStage: string): string;
begin
  if AStage = 'resolve' then Result := '配置解析'
  else if AStage = 'auth' then Result := '鉴权'
  else if AStage = 'request' then Result := '请求'
  else if AStage = 'upstream' then Result := '上游'
  else if AStage = 'response' then Result := '响应'
  else if AStage = 'timeout' then Result := '超时'
  else if AStage = 'protocol' then Result := '协议'
  else if AStage = 'bridge' then Result := '桥接器'
  else Result := AStage;
end;

// 探测传输协议标识 → 保留协议名称（中英混排仍易于辨认，不强行音译）。
function TMainForm.ProbeTransportName(const ATransport: string): string;
begin
  if ATransport = 'anthropic' then Result := 'Anthropic'
  else if ATransport = 'gemini-interactions' then Result := 'Gemini Interactions'
  else if ATransport = 'openai-chat' then Result := 'OpenAI Chat'
  else if ATransport = 'openai-responses' then Result := 'OpenAI Responses'
  else if ATransport = 'antigravity-ls' then Result := 'LS 自主执行'
  else if ATransport = 'gemini' then Result := 'Gemini'
  else Result := ATransport;
end;

// 翻译桥接器稳定拥有的探测报错文案；上游/提供程序原文一律原样保留，
// 避免对英文免费文本做机器式替换或误改技术详情。
function TMainForm.LocalizeProbeMessage(const AMessage: string): string;
const
  CUnknownProfilePrefix = 'Unknown provider profile ''';
  CTimeoutPrefix = 'Profile probe timed out after ';
  CCannotReadPrefix = 'Cannot read probe response: ';
begin
  Result := AMessage;
  if Result = '' then
    Exit;
  if SameText(Result, 'Cannot read provider routing state') then
    Exit('无法读取提供程序路由状态');
  if SameText(Result,
    'Profile probe received an invalid Anthropic message response') then
    Exit('探测响应不是有效的 Anthropic message');
  if SameText(Result,
    'LocalGemini requires GEMINI_API_KEY or GEMINI_BRIDGE_API_KEY_PROFILE') then
    Exit('本地 Gemini 需要 GEMINI_API_KEY 或 GEMINI_BRIDGE_API_KEY_PROFILE');
  if SameText(Result, 'Profile probe failed without a structured error message') then
    Exit('探测失败：未收到结构化错误信息');
  // 以下三条带动态片段（文件名 / 毫秒数 / 底层错误），逐段译出并保留细节。
  if Copy(Result, 1, Length(CUnknownProfilePrefix)) = CUnknownProfilePrefix then
    Exit('未找到模型配置：' +
      Copy(Result, Length(CUnknownProfilePrefix) + 1,
        Length(Result) - Length(CUnknownProfilePrefix) - 1));
  if Copy(Result, 1, Length(CTimeoutPrefix)) = CTimeoutPrefix then
    Exit('探测超时（上限 ' +
      Copy(Result, Length(CTimeoutPrefix) + 1,
        Length(Result) - Length(CTimeoutPrefix) - 3) + ' ms）');
  if Copy(Result, 1, Length(CCannotReadPrefix)) = CCannotReadPrefix then
    Exit('无法读取探测响应：' +
      Copy(Result, Length(CCannotReadPrefix) + 1, MaxInt));
end;

// 成功文案按探针返回的 transport / upstream_status 渲染为自然中文，
// 不再直接显示后端的英文通用成功消息。
function TMainForm.ProbeSuccessSummary(const ATransport, AUpstreamStatus,
  ARawMessage: string): string;
begin
  Result := ProbeTransportName(ATransport);
  if Result <> '' then
  begin
    Result := Result + ' 上游响应有效';
    if AUpstreamStatus <> '' then
      Result := Result + '（HTTP ' + AUpstreamStatus + '）';
    Exit;
  end;
  // 旧版桥接器未返回 transport：退回后端成功文案，避免丢失信息。
  if ARawMessage <> '' then
    Result := ARawMessage
  else
    Result := '上游响应有效';
end;

// 探测网络分阶段耗时。响应头阶段包含 DNS、建连、代理隧道、TLS、
// 重试和上游排队；连接复用时不把它误报为单独的“连接耗时”。
function TMainForm.ProbeNetworkSummary(const AProxyMode, AProxyUrl,
  AResponseHeadersMs, AFirstByteMs, AResponseBodyMs,
  ATotalMs: string): string;
  procedure AddPart(const ALabel, AValue: string);
  begin
    if AValue = '' then
      Exit;
    if Result <> '' then
      Result := Result + '；';
    Result := Result + ALabel + '：' + AValue;
  end;
begin
  Result := '';
  if SameText(AProxyMode, 'proxy') then
  begin
    if AProxyUrl <> '' then
      AddPart('代理', AProxyUrl)
    else
      AddPart('代理模式', '代理');
  end
  else if SameText(AProxyMode, 'direct') then
    AddPart('代理模式', '直连');
  if AResponseHeadersMs <> '' then
    AddPart('连接/响应头', AResponseHeadersMs + ' ms');
  if AFirstByteMs <> '' then
    AddPart('首包', AFirstByteMs + ' ms');
  if AResponseBodyMs <> '' then
    AddPart('响应体完成', AResponseBodyMs + ' ms');
  if ATotalMs <> '' then
    AddPart('总耗时', ATotalMs + ' ms');
end;

procedure TMainForm.TestProfileAsync(const AFileName: string);
begin
  // 探测使用独立在途状态：只禁用“测试”按钮并拒绝重复探测，
  // 不进入全局 FBusy，窗口与其余操作保持可用。
  if FClosing or FBusy or FTestInFlight then
    Exit;
  FTestInFlight := True;
  FTestButton.Enabled := False;
  FTestResultLabel.Caption := '测试中：' + AFileName;
  FTestResultLabel.Font.Color := $00606060;
  Inc(FWorkerCount);
  TThread.CreateAnonymousThread(
    procedure
    var
      LClient: THTTPClient;
      LRequest: TJSONObject;
      LHeaders: TNetHeaders;
      LStream: TStringStream;
      LResponse: IHTTPResponse;
      LValue: TJSONValue;
      LJson: TJSONObject;
      LNetwork: TJSONObject;
      LResponseText: string;
      LStatus: string;
      LLatencyMs: string;
      LTransport: string;
      LUpstreamStatus: string;
      LMessage: string;
      LStage: string;
      LProxyMode: string;
      LProxyUrl: string;
      LResponseHeadersMs: string;
      LFirstByteMs: string;
      LResponseBodyMs: string;
      LTotalMs: string;
      LError: string;
    begin
      LError := '';
      LStatus := '';
      LLatencyMs := '';
      LMessage := '';
      LStage := '';
      LProxyMode := '';
      LProxyUrl := '';
      LResponseHeadersMs := '';
      LFirstByteMs := '';
      LResponseBodyMs := '';
      LTotalMs := '';
      try
        LClient := NewHttpClient(PROBE_RESPONSE_TIMEOUT_MS);
        try
          SetLength(LHeaders, 2);
          LHeaders[0].Name := 'Content-Type';
          LHeaders[0].Value := 'application/json';
          LHeaders[1].Name := 'Authorization';
          LHeaders[1].Value := 'Bearer ' + FLocalAuthToken;
          LRequest := TJSONObject.Create;
          try
            LRequest.AddPair('file', AFileName);
            LStream := TStringStream.Create(LRequest.ToJSON, TEncoding.UTF8);
          finally
            LRequest.Free;
          end;
          try
            LResponse := LClient.Post(
              FBridgeUrl + '/admin/profiles/test',
              LStream,
              nil,
              LHeaders
            );
            LResponseText := LResponse.ContentAsString(TEncoding.UTF8);
            LValue := TJSONObject.ParseJSONValue(LResponseText);
            if not (LValue is TJSONObject) then
            begin
              LValue.Free;
              raise Exception.CreateFmt('HTTP %d: %s', [
                LResponse.StatusCode,
                LResponseText
              ]);
            end;
            LJson := TJSONObject(LValue);
            try
              LStatus := JsonText(LJson, 'status');
              LLatencyMs := JsonText(LJson, 'latency_ms');
              LTransport := JsonText(LJson, 'transport');
              LUpstreamStatus := JsonText(LJson, 'upstream_status');
              if LJson.Values['network'] is TJSONObject then
              begin
                LNetwork := TJSONObject(LJson.Values['network']);
                LProxyMode := JsonText(LNetwork, 'proxy_mode');
                LProxyUrl := JsonText(LNetwork, 'proxy_url');
                LResponseHeadersMs := JsonText(LNetwork, 'response_headers_ms');
                LFirstByteMs := JsonText(LNetwork, 'first_byte_ms');
                LResponseBodyMs := JsonText(LNetwork, 'response_body_ms');
                LTotalMs := JsonText(LNetwork, 'total_ms');
              end;
              if LJson.Values['error'] is TJSONObject then
              begin
                LStage := JsonText(TJSONObject(LJson.Values['error']), 'stage');
                LMessage := JsonText(TJSONObject(LJson.Values['error']), 'message');
              end
              else
              begin
                LMessage := JsonText(LJson, 'message');
                if LMessage = '' then
                  LMessage := JsonText(LJson, 'error');
              end;
              if (LStatus = '') and
                 ((LResponse.StatusCode < 200) or
                  (LResponse.StatusCode >= 300)) then
                LStatus := 'error';
              if not SameText(LStatus, 'ok') then
              begin
                if LStage = '' then
                  LStage := 'bridge';
                if LMessage = '' then
                  LMessage := Format(
                    '桥接器返回 HTTP %d',
                    [LResponse.StatusCode]
                  );
              end;
            finally
              LJson.Free;
            end;
          finally
            LStream.Free;
          end;
        finally
          LClient.Free;
        end;
      except
        on E: Exception do
          LError := E.Message;
      end;
      QueueToMain(
        procedure
        var
          LDisplay: string;
          LSummary: string;
          LNetworkSummary: string;
          LStageZh: string;
          LMessageZh: string;
        begin
          // AFileName 是发起探测时捕获的原始文件：探测期间可更改选中项，
          // 结果与日志仍归属原模型；若其他操作仍在忙（FBusy）则不恢复按钮。
          FTestInFlight := False;
          FTestButton.Enabled :=
            (FStatusLabel.Tag = 1) and not FBusy;
          LNetworkSummary := ProbeNetworkSummary(
            LProxyMode, LProxyUrl, LResponseHeadersMs,
            LFirstByteMs, LResponseBodyMs, LTotalMs
          );
          if LError <> '' then
          begin
            FTestResultLabel.Caption := '测试失败：' + LError;
            FTestResultLabel.Font.Color := $00800000;
            AppendLog('模型连通性测试失败：' + AFileName + ' -- ' + LError);
          end
          else if SameText(LStatus, 'ok') then
          begin
            FTestResultLabel.Caption := '测试通过：' + LLatencyMs + ' ms';
            FTestResultLabel.Font.Color := $00106410;
            LSummary := ProbeSuccessSummary(LTransport, LUpstreamStatus, LMessage);
            if LNetworkSummary <> '' then
              LSummary := LSummary + '；' + LNetworkSummary;
            AppendLog(
              Format('模型连通性测试通过：%s（%s）', [
                AFileName, LSummary
              ])
            );
          end
          else
          begin
            LStageZh := ProbeStageName(LStage);
            LMessageZh := LocalizeProbeMessage(LMessage);
            LDisplay := '测试失败：' + LStageZh + '：' + LMessageZh;
            if Length(LDisplay) > 24 then
              LDisplay := Copy(LDisplay, 1, 24) + '…';
            FTestResultLabel.Caption := LDisplay;
            FTestResultLabel.Font.Color := $00800000;
            if LNetworkSummary <> '' then
              AppendLog(
                Format('模型连通性测试失败：%s -- %s：%s（%s）', [
                  AFileName, LStageZh, LMessageZh, LNetworkSummary
                ])
              )
            else
              AppendLog(
                Format('模型连通性测试失败：%s -- %s：%s', [
                  AFileName, LStageZh, LMessageZh
                ])
              );
          end;
        end);
    end).Start;
end;

procedure TMainForm.ProfileDblClick(Sender: TObject);
var
  LPos: TPoint;
  LItem: TListItem;
begin
  LPos := FProfileList.ScreenToClient(Mouse.CursorPos);
  LItem := FProfileList.GetItemAt(LPos.X, LPos.Y);
  if (LItem <> nil) and (LItem.Index >= 0) and
     (LItem.Index < FProfileFiles.Count) then
    SwitchProfileAsync(FProfileFiles[LItem.Index]);
end;

function TMainForm.FindBridgeRoot: string;
var
  I: Integer;
begin
  Result := ExcludeTrailingPathDelimiter(ExtractFilePath(ParamStr(0)));
  for I := 0 to 5 do
  begin
    if FileExists(Result + '\scripts\start-bridge.ps1') then
      Exit;
    Result := ExtractFileDir(Result);
  end;
  Result := ExcludeTrailingPathDelimiter(ExtractFilePath(ParamStr(0)));
end;

function TMainForm.RunPowerShellScript(const AScriptPath: string): Cardinal;
var
  LCommandLine: string;
  LPowerShell: string;
  LStartupInfo: TStartupInfo;
  LProcessInfo: TProcessInformation;
  LWaitResult: DWORD;
begin
  LPowerShell :=
    GetEnvironmentVariable('SystemRoot') +
    '\System32\WindowsPowerShell\v1.0\powershell.exe';
  LCommandLine := Format(
    '"%s" -NoLogo -NoProfile -ExecutionPolicy Bypass -File "%s"',
    [LPowerShell, AScriptPath]
  );
  UniqueString(LCommandLine);
  ZeroMemory(@LStartupInfo, SizeOf(LStartupInfo));
  ZeroMemory(@LProcessInfo, SizeOf(LProcessInfo));
  LStartupInfo.cb := SizeOf(LStartupInfo);
  LStartupInfo.dwFlags := STARTF_USESHOWWINDOW;
  LStartupInfo.wShowWindow := SW_HIDE;

  if not CreateProcess(
    nil,
    PChar(LCommandLine),
    nil,
    nil,
    False,
    CREATE_NO_WINDOW,
    nil,
    PChar(FBridgeRoot),
    LStartupInfo,
    LProcessInfo
  ) then
    RaiseLastOSError;
  try
    LWaitResult := WaitForSingleObject(
      LProcessInfo.hProcess,
      SCRIPT_PROCESS_TIMEOUT_MS
    );
    if LWaitResult = WAIT_FAILED then
      RaiseLastOSError;
    if LWaitResult = WAIT_TIMEOUT then
      raise Exception.Create('服务脚本运行超过 5 分钟，仍未结束。');
    if LWaitResult <> WAIT_OBJECT_0 then
      raise Exception.CreateFmt('等待服务脚本失败，状态码 %d。', [LWaitResult]);
    if not GetExitCodeProcess(LProcessInfo.hProcess, Result) then
      RaiseLastOSError;
  finally
    CloseHandle(LProcessInfo.hThread);
    CloseHandle(LProcessInfo.hProcess);
  end;
end;

procedure TMainForm.StartStopAsync(const AStart: Boolean);
begin
  if FClosing then
    Exit;
  SetBusy(True);
  Inc(FWorkerCount);
  TThread.CreateAnonymousThread(
    procedure
    var
      LExitCode: Cardinal;
      LError: string;
      LScript: string;
      LLogOk: string;
      LLogFail: string;
    begin
      LError := '';
      if AStart then
      begin
        LScript := FBridgeRoot + '\scripts\start-bridge.ps1';
        LLogOk := '桥接器已启动。';
        LLogFail := '启动失败：';
      end
      else
      begin
        LScript := FBridgeRoot + '\scripts\stop-bridge.ps1';
        LLogOk := '桥接器已停止。';
        LLogFail := '停止失败：';
      end;
      try
        LExitCode := RunPowerShellScript(LScript);
        if LExitCode <> 0 then
          raise Exception.CreateFmt('脚本返回错误码 %d', [LExitCode]);
        if AStart then
          Sleep(500);
      except
        on E: Exception do
          LError := E.Message;
      end;
      QueueToMain(
        procedure
        begin
          SetBusy(False);
          if LError <> '' then
          begin
            AppendLog(LLogFail + LError);
            MessageDlg(LError, mtError, [mbOK], 0);
          end
          else
            AppendLog(LLogOk);
        end);
    end).Start;
end;

procedure TMainForm.StartClick(Sender: TObject);
begin
  StartStopAsync(True);
end;

procedure TMainForm.StopClick(Sender: TObject);
begin
  StartStopAsync(False);
end;

procedure TMainForm.PollTimer(Sender: TObject);
begin
  RefreshStatusAsync(True);
end;

end.
