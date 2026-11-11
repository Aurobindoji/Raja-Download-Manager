#define MyAppName "RAJA Download Manager"
#define MyAppVersion "0.1.0"
#define MyAppPublisher "RAJA contributors"

[Setup]
AppId={{67EA6F1B-9978-4740-9D41-79AD78E3A234}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
DefaultDirName={localappdata}\Programs\Raja
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\target\installer
OutputBaseFilename=Raja-{#MyAppVersion}-Setup-x64
SetupIconFile=raja.ico
UninstallDisplayIcon={app}\raja.ico
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
CloseApplications=no
RestartApplications=no
LicenseFile=..\LICENSE
SetupLogging=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Additional shortcuts:"
Name: "browserextension"; Description: "Open browser extension loading instructions"; GroupDescription: "Browser integration:"

[Files]
Source: "..\target\release\raja-ui.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\target\release\raja-core.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\target\release\raja-browser-host.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "raja.ico"; DestDir: "{app}"; Flags: ignoreversion
Source: "raja.ico"; DestDir: "{app}"; DestName: "raja-shield.ico"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\browser-extension\*"; DestDir: "{app}\browser-extension"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{autoprograms}\{#MyAppName}"; Filename: "{app}\raja-ui.exe"; IconFilename: "{app}\raja-shield.ico"
Name: "{autodesktop}\RAJA"; Filename: "{app}\raja-ui.exe"; IconFilename: "{app}\raja-shield.ico"; Tasks: desktopicon

[Run]
Filename: "{app}\raja-ui.exe"; Description: "Launch RAJA Download Manager"; Flags: postinstall nowait skipifsilent
Filename: "{cmd}"; Parameters: "/c start """" ""{app}\README.md"""; Description: "Open README for browser extension setup"; Tasks: browserextension; Flags: postinstall nowait skipifsilent

[Registry]
Root: HKCU; Subkey: "Software\Classes\raja"; ValueType: string; ValueName: ""; ValueData: "URL:RAJA download link"
Root: HKCU; Subkey: "Software\Classes\raja"; ValueType: string; ValueName: "URL Protocol"; ValueData: ""
Root: HKCU; Subkey: "Software\Classes\raja\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\raja-ui.exe"" ""%1"""
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "RAJA Download Manager"; ValueData: """{app}\raja-ui.exe"" --background"; Flags: uninsdeletevalue

[UninstallDelete]
Type: files; Name: "{app}\com.raja.browser.json"

[Code]
const
  BrowserHostName = 'com.raja.browser';
  BrowserExtensionOrigin = 'chrome-extension://fgloankgfdmmebbalhnnloijbeihdbgc/';

function HexDigit(Value: Integer): Char;
begin
  if Value < 10 then
    Result := Chr(Ord('0') + Value)
  else
    Result := Chr(Ord('A') + Value - 10);
end;

function JsonEscape(const Value: String): String;
var
  I: Integer;
  Code: Integer;
  Character: Char;
begin
  Result := '';
  for I := 1 to Length(Value) do
  begin
    Character := Value[I];
    if Character = '\' then
      Result := Result + '\\'
    else if Character = '"' then
      Result := Result + '\"'
    else if Ord(Character) > 127 then
    begin
      Code := Ord(Character);
      Result := Result + '\u' +
        HexDigit(Code div 4096) +
        HexDigit((Code div 256) mod 16) +
        HexDigit((Code div 16) mod 16) +
        HexDigit(Code mod 16);
    end
    else
      Result := Result + Character;
  end;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  ExistingCommand: String;
  RajaCommand: String;
  ResultCode: Integer;
begin
  Result := '';
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM raja-browser-host.exe /T', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM raja-ui.exe /T', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM raja-core.exe /T', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  RajaCommand := '"' + ExpandConstant('{app}\raja-ui.exe') + '" "%1"';
  if RegQueryStringValue(
    HKEY_CURRENT_USER,
    'Software\Classes\raja\shell\open\command',
    '',
    ExistingCommand
  ) and (CompareText(ExistingCommand, RajaCommand) <> 0) then
  begin
    if MsgBox(
      'Another application is registered for the raja: link protocol. Replace that registration with RAJA?',
      mbConfirmation,
      MB_YESNO
    ) <> IDYES then
      Result := 'Installation cancelled because the raja: protocol is registered to another application.';
  end;
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  ManifestPath: String;
  HostPath: String;
  ManifestContents: String;
begin
  if CurStep = ssPostInstall then
  begin
    ManifestPath := ExpandConstant('{app}\com.raja.browser.json');
    HostPath := JsonEscape(ExpandConstant('{app}\raja-browser-host.exe'));
    ManifestContents :=
      '{"name":"' + BrowserHostName +
      '","description":"RAJA browser download bridge","path":"' + HostPath +
      '","type":"stdio","allowed_origins":["' + BrowserExtensionOrigin + '"]}';
    if not SaveStringToFile(ManifestPath, ManifestContents, False) then
      RaiseException('Could not write the RAJA browser bridge manifest.');

    if not RegWriteStringValue(
      HKEY_CURRENT_USER,
      'Software\Google\Chrome\NativeMessagingHosts\' + BrowserHostName,
      '',
      ManifestPath
    ) then
      RaiseException('Could not register the RAJA browser bridge for Chrome.');
    if not RegWriteStringValue(
      HKEY_CURRENT_USER,
      'Software\Microsoft\Edge\NativeMessagingHosts\' + BrowserHostName,
      '',
      ManifestPath
    ) then
      RaiseException('Could not register the RAJA browser bridge for Edge.');
    if not RegWriteStringValue(
      HKEY_CURRENT_USER,
      'Software\BraveSoftware\Brave-Browser\NativeMessagingHosts\' + BrowserHostName,
      '',
      ManifestPath
    ) then
      RaiseException('Could not register the RAJA browser bridge for Brave.');
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  ExistingCommand: String;
  RajaCommand: String;
  ManifestPath: String;
  ExistingManifest: String;
begin
  if CurUninstallStep = usUninstall then
  begin
    RajaCommand := '"' + ExpandConstant('{app}\raja-ui.exe') + '" "%1"';
    if RegQueryStringValue(
      HKEY_CURRENT_USER,
      'Software\Classes\raja\shell\open\command',
      '',
      ExistingCommand
    ) and (CompareText(ExistingCommand, RajaCommand) = 0) then
    begin
      if not RegDeleteKeyIncludingSubkeys(
        HKEY_CURRENT_USER,
        'Software\Classes\raja'
      ) then
        MsgBox(
          'RAJA could not remove its raja: browser link registration.',
          mbError,
          MB_OK
        );
    end;
    ManifestPath := ExpandConstant('{app}\com.raja.browser.json');
    if RegQueryStringValue(
      HKEY_CURRENT_USER,
      'Software\Google\Chrome\NativeMessagingHosts\' + BrowserHostName,
      '',
      ExistingManifest
    ) and (CompareText(ExistingManifest, ManifestPath) = 0) then
      RegDeleteKeyIncludingSubkeys(
        HKEY_CURRENT_USER,
        'Software\Google\Chrome\NativeMessagingHosts\' + BrowserHostName
      );
    if RegQueryStringValue(
      HKEY_CURRENT_USER,
      'Software\Microsoft\Edge\NativeMessagingHosts\' + BrowserHostName,
      '',
      ExistingManifest
    ) and (CompareText(ExistingManifest, ManifestPath) = 0) then
      RegDeleteKeyIncludingSubkeys(
        HKEY_CURRENT_USER,
        'Software\Microsoft\Edge\NativeMessagingHosts\' + BrowserHostName
      );
    if RegQueryStringValue(
      HKEY_CURRENT_USER,
      'Software\BraveSoftware\Brave-Browser\NativeMessagingHosts\' + BrowserHostName,
      '',
      ExistingManifest
    ) and (CompareText(ExistingManifest, ManifestPath) = 0) then
      RegDeleteKeyIncludingSubkeys(
        HKEY_CURRENT_USER,
        'Software\BraveSoftware\Brave-Browser\NativeMessagingHosts\' + BrowserHostName
      );
  end;
end;
