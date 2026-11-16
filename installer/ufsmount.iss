; ufsmount setup (Inno Setup 7). Build: ISCC /DAppVersion=x.y.z installer\ufsmount.iss
; Expects temp\release\ufsmount.exe and temp\release\ufsmount-tray.exe.

#include "winfsp.iss"

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

#define AppGuid "9A3E6C51-2F4B-4E8D-9C27-5B1D3F0A8E64"
#define UninstallKey "SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\{" + AppGuid + "}_is1"
#define LauncherKey "SOFTWARE\WinFsp\Services\ufs"

[Setup]
AppId={{{#AppGuid}}
AppName=ufsmount
AppVersion={#AppVersion}
AppVerName=ufsmount {#AppVersion}
AppPublisher=poeggi
AppPublisherURL=https://github.com/poeggi/winfsp-ufs
AppSupportURL=https://github.com/poeggi/winfsp-ufs/issues
VersionInfoVersion={#AppVersion}
DefaultDirName={autopf}\ufsmount
DisableDirPage=yes
DisableProgramGroupPage=yes
DirExistsWarning=no
PrivilegesRequired=admin
MinVersion=10.0
SetupArchitecture=x64
ArchitecturesAllowed=x64os
ArchitecturesInstallIn64BitMode=x64os
OutputDir=..\temp\installer
OutputBaseFilename=ufsmount-{#AppVersion}-setup
SetupIconFile=..\logos\ufsmount.ico
UninstallDisplayIcon={app}\ufsmount-tray.exe
UninstallDisplayName=ufsmount
WizardStyle=modern
; Sizes per DPI as listed for these directives in the Inno Setup help.
WizardImageFile=..\logos\wizard\large-*.png
WizardSmallImageFile=..\logos\wizard\small-*.png
Compression=lzma2
SolidCompression=yes
SetupLogging=yes
UsePreviousTasks=yes
CloseApplications=no
ChangesEnvironment=yes

[Tasks]
Name: logon; Description: "Start ufsmount when I sign in"; Flags: unchecked
Name: path; Description: "Add ufsmount to the PATH, for the command line"
Name: launcher; Description: "Register with the WinFsp launcher, for net use \\ufs\..."; Flags: unchecked

[Files]
Source: "..\temp\release\ufsmount.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\temp\release\ufsmount-tray.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"
Source: "..\THIRD-PARTY-NOTICES.txt"; DestDir: "{app}"
Source: "..\licenses\GPL-3.0.txt"; DestDir: "{app}"
Source: "..\licenses\Apache-2.0.txt"; DestDir: "{app}"

[Icons]
Name: "{autoprograms}\ufsmount"; Filename: "{app}\ufsmount-tray.exe"; Comment: "Mount FreeBSD UFS disks read-only"

[Run]
Filename: "{app}\ufsmount-tray.exe"; Description: "Start ufsmount"; Flags: postinstall nowait skipifsilent runasoriginaluser

[Code]
var
  TasksChecked: Boolean;
  RestartNeeded: Boolean;

{ --- running processes --- }

procedure RunHidden(const Exe, Params: String; var Code: Integer);
begin
  if not Exec(Exe, Params, '', SW_HIDE, ewWaitUntilTerminated, Code) then
    Code := -1;
  Log(Format('%s %s: exit code %d', [Exe, Params, Code]));
end;

{ Ask a running tray to unmount and quit, then end whatever still runs. Both
  are read-only, and WinFsp drops a mount when its process ends. }
procedure StopRunning;
var
  Code: Integer;
  Tray: String;
begin
  Tray := ExpandConstant('{app}\ufsmount-tray.exe');
  if FileExists(Tray) then
    RunHidden(Tray, '--quit', Code);
  RunHidden(ExpandConstant('{sys}\taskkill.exe'), '/F /IM ufsmount-tray.exe', Code);
  RunHidden(ExpandConstant('{sys}\taskkill.exe'), '/F /IM ufsmount.exe', Code);
end;

procedure RunOrWarn(const Exe, Params, What: String);
var
  Code: Integer;
begin
  RunHidden(Exe, Params, Code);
  if Code <> 0 then
    SuppressibleMsgBox(Format('ufsmount: %s failed (exit code %d).', [What, Code]),
      mbError, MB_OK, IDOK);
end;

{ --- WinFsp --- }

function WinFspOk: Boolean;
var
  Dir: String;
  MS, LS, Major, Minor, Build: Cardinal;
begin
  Result := False;
  if not RegQueryStringValue(HKLM32, 'SOFTWARE\WinFsp', 'InstallDir', Dir) then begin
    Log('WinFsp: not installed');
    exit;
  end;
  if not GetVersionNumbers(AddBackslash(Dir) + 'bin\winfsp-x64.dll', MS, LS) then begin
    Log('WinFsp: no version in ' + Dir);
    exit;
  end;
  Major := MS shr 16;
  Minor := MS and $FFFF;
  Build := LS shr 16;
  Log(Format('WinFsp: %d.%d.%d in %s', [Major, Minor, Build, Dir]));
  Result := (Major > {#WinFspMinMajor}) or
    ((Major = {#WinFspMinMajor}) and ((Minor > {#WinFspMinMinor}) or
      ((Minor = {#WinFspMinMinor}) and (Build >= {#WinFspMinBuild}))));
end;

{ Copy the WinFsp MSI next to the setup exe to Msi, if it is the expected one. }
function CopyLocalMsi(const Msi: String): Boolean;
var
  Src: String;
begin
  Result := False;
  Src := ExpandConstant('{src}\{#WinFspMsi}');
  if not FileExists(Src) then
    exit;
  if not CopyFile(Src, Msi, False) then begin
    Log('WinFsp: cannot copy ' + Src);
    exit;
  end;
  try
    Result := GetSHA256OfFile(Msi) = '{#WinFspSha256}';
  except
    Result := False;
  end;
  if Result then
    Log('WinFsp: using ' + Src)
  else
    Log('WinFsp: not the expected MSI: ' + Src);
end;

{ Put the checked WinFsp MSI into Setup's temporary folder: admin-only when
  elevated, unlike perhaps the setup's own folder. Msi gets its path. Returns
  an error text, or ''. The local copy has its own name: CopyFile keeps a
  read-only attribute, which would block the download. }
function GetWinFspMsi(var Msi: String): String;
begin
  Result := '';
  Msi := ExpandConstant('{tmp}\local-{#WinFspMsi}');
  if CopyLocalMsi(Msi) then
    exit;
  Msi := ExpandConstant('{tmp}\{#WinFspMsi}');
  try
    DownloadTemporaryFile('{#WinFspUrl}', '{#WinFspMsi}', '{#WinFspSha256}', nil);
  except
    Result := 'WinFsp could not be downloaded: ' + GetExceptionMessage + #13#10#13#10 +
      'Install WinFsp {#WinFspMinMajor}.{#WinFspMinMinor}.{#WinFspMinBuild} or later from ' +
      'https://github.com/winfsp/winfsp/releases and run this setup again.';
  end;
end;

{ Run the MSI from GetWinFspMsi. Returns an error text, or ''.
  A restart request of the MSI (3010) is passed on through NeedRestart. }
function InstallWinFsp(const Msi: String): String;
var
  MsiLog, UI: String;
  Code: Integer;
begin
  Result := '';
  { Next to the setup log: the temporary folder is deleted when setup ends. }
  MsiLog := ExpandConstant('{log}');
  if MsiLog = '' then
    MsiLog := ExpandConstant('{commonappdata}\ufsmount-winfsp-setup.txt')
  else
    MsiLog := MsiLog + '.winfsp.txt';
  if WizardSilent then UI := '/qn' else UI := '/passive';
  if not Exec(ExpandConstant('{sys}\msiexec.exe'),
    Format('/i "%s" /norestart %s /l*v "%s"', [Msi, UI, MsiLog]),
    '', SW_SHOW, ewWaitUntilTerminated, Code) then
    Code := -1;
  Log(Format('WinFsp setup: exit code %d, log %s', [Code, MsiLog]));
  case Code of
    0: ;
    3010: begin
      { The new DLL is in place after the restart; do not check it now. }
      RestartNeeded := True;
      exit;
    end;
    1618: begin
      Result := 'Another installation is running. Wait until it ends and run this setup again.';
      exit;
    end;
  else
    begin
      Result := Format('The WinFsp setup failed with exit code %d. Its log: %s', [Code, MsiLog]);
      exit;
    end;
  end;
  if not WinFspOk then
    Result := 'WinFsp is still missing or too old after its setup.';
end;

{ --- setup steps --- }

function HasParam(const Name: String): Boolean;
var
  I: Integer;
begin
  Result := False;
  for I := 1 to ParamCount do
    if CompareText(ParamStr(I), Name) = 0 then
      Result := True;
end;

function HasParamPrefix(const Prefix: String): Boolean;
var
  I: Integer;
begin
  Result := False;
  for I := 1 to ParamCount do
    if CompareText(Copy(ParamStr(I), 1, Length(Prefix)), Prefix) = 0 then
      Result := True;
end;

{ Installing an older version over a newer one needs a Yes, or /ALLOWDOWNGRADE
  when message boxes are suppressed. }
function InitializeSetup: Boolean;
var
  Installed: String;
  Have, Mine: Int64;
begin
  Result := True;
  if not RegQueryStringValue(HKLM64, '{#UninstallKey}', 'DisplayVersion', Installed) then
    exit;
  if not StrToVersion(Installed, Have) or not StrToVersion('{#AppVersion}', Mine) then begin
    Log('Version check skipped: installed version ' + Installed);
    exit;
  end;
  if ComparePackedVersion(Mine, Have) >= 0 then
    exit;
  Log(Format('Downgrade from %s to {#AppVersion}', [Installed]));
  if HasParam('/ALLOWDOWNGRADE') then
    exit;
  Result := SuppressibleMsgBox(Format('ufsmount %s is installed. This setup has the older ' +
    'version {#AppVersion}.' + #13#10#13#10 + 'Install the older version anyway?', [Installed]),
    mbConfirmation, MB_YESNO or MB_DEFBUTTON2, IDNO) = IDYES;
end;

procedure InitializeWizard;
var
  Offset: Integer;
begin
  { Offset is the room left of a checkbox in these lists and between it and
    its text. Inno Setup 7.1.1 reserves too little room for the checkbox at
    high DPI and clips it (fixed upstream after 7.1.1), so the growth of the
    13 px checkbox is added. }
  Offset := ScaleX(8) + ScaleX(13) - 13;
  WizardForm.ComponentsList.Offset := Offset;
  WizardForm.TasksList.Offset := Offset;
  WizardForm.RunList.Offset := Offset;
end;

{ True when the installed state decides the launcher and logon tasks: the
  command line sets no tasks. }
function KeepInstalledTasks: Boolean;
begin
  Result := not HasParamPrefix('/TASKS=') and not HasParamPrefix('/MERGETASKS=');
end;

{ The tray's "Start at sign-in" of the installed version: '1', '0' or ''.
  Only an installed version has made its folder admin-only. }
function InstalledSignIn: String;
var
  Path: String;
  S: AnsiString;
begin
  Result := '';
  if not RegKeyExists(HKLM64, '{#UninstallKey}') then
    exit;
  Path := ExpandConstant('{commonappdata}\ufsmount\start-at-sign-in');
  if not LoadStringFromFile(Path, S) then begin
    Log('Start at sign-in: cannot read ' + Path);
    exit;
  end;
  if (S = '1') or (S = '0') then
    Result := S;
  Log('Start at sign-in of the installed version: "' + S + '"');
end;

{ A task as chosen. A silent install without tasks on the command line
  keeps the installed launcher registration and "Start at sign-in". }
function TaskWanted(Task: String): Boolean;
var
  SignIn: String;
begin
  Result := WizardIsTaskSelected(Task);
  if not WizardSilent or not KeepInstalledTasks then
    exit;
  if Task = 'launcher' then
    Result := Result or RegKeyExists(HKLM32, '{#LauncherKey}')
  else if Task = 'logon' then begin
    SignIn := InstalledSignIn;
    if SignIn <> '' then
      Result := SignIn = '1';
  end;
end;

procedure CurPageChanged(CurPageID: Integer);
var
  SignIn: String;
begin
  { Show the installed launcher registration and "Start at sign-in". }
  if (CurPageID = wpSelectTasks) and not TasksChecked then begin
    TasksChecked := True;
    if KeepInstalledTasks then begin
      if RegKeyExists(HKLM32, '{#LauncherKey}') then
        WizardSelectTasks('launcher');
      SignIn := InstalledSignIn;
      if SignIn = '1' then
        WizardSelectTasks('logon')
      else if SignIn = '0' then
        WizardSelectTasks('!logon');
    end;
  end;
end;

{ The MSI is in place before the tray and its mounts stop, so a failed
  download leaves them running. }
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  NeedWinFsp: Boolean;
  Msi: String;
begin
  Result := '';
  NeedWinFsp := not WinFspOk;
  if NeedWinFsp then begin
    Result := GetWinFspMsi(Msi);
    if Result <> '' then
      exit;
  end;
  StopRunning;
  if NeedWinFsp then
    Result := InstallWinFsp(Msi);
end;

function NeedRestart: Boolean;
begin
  Result := RestartNeeded;
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  Params: String;
  Code: Integer;
begin
  if CurStep <> ssPostInstall then
    exit;
  Params := '--install-task';
  if TaskWanted('logon') then
    Params := Params + ' --logon';
  RunOrWarn(ExpandConstant('{app}\ufsmount-tray.exe'), Params, 'creating the start task');
  if TaskWanted('launcher') then
    RunOrWarn(ExpandConstant('{app}\ufsmount.exe'), 'register', 'registering with the WinFsp launcher')
  else if RegKeyExists(HKLM32, '{#LauncherKey}') then
    RunHidden(ExpandConstant('{app}\ufsmount.exe'), 'unregister', Code);
  { The exe keeps the PATH value's registry type; Pascal script cannot read it.
    --remove-from-path takes back only what --add-to-path added. }
  if WizardIsTaskSelected('path') then
    RunOrWarn(ExpandConstant('{app}\ufsmount-tray.exe'), '--add-to-path', 'adding ufsmount to the PATH')
  else
    RunHidden(ExpandConstant('{app}\ufsmount-tray.exe'), '--remove-from-path', Code);
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Code: Integer;
begin
  if CurUninstallStep <> usUninstall then
    exit;
  StopRunning;
  RunHidden(ExpandConstant('{app}\ufsmount-tray.exe'), '--remove-task', Code);
  RunHidden(ExpandConstant('{app}\ufsmount.exe'), 'unregister', Code);
  RunHidden(ExpandConstant('{app}\ufsmount-tray.exe'), '--remove-from-path', Code);
end;
