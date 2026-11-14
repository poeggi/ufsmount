; ufsmount setup (Inno Setup 7). Build: ISCC /DAppVersion=x.y.z installer\ufsmount.iss
; Expects target\release\ufsmount.exe and target\release\ufsmount-tray.exe.

#include "winfsp.iss"

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

[Setup]
AppId={{9A3E6C51-2F4B-4E8D-9C27-5B1D3F0A8E64}
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
OutputDir=..\target\installer
OutputBaseFilename=ufsmount-{#AppVersion}-setup
SetupIconFile=..\assets\ufsmount.ico
UninstallDisplayIcon={app}\ufsmount-tray.exe
UninstallDisplayName=ufsmount
WizardStyle=modern
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
Source: "..\target\release\ufsmount.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\target\release\ufsmount-tray.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"
Source: "..\THIRD-PARTY-NOTICES.txt"; DestDir: "{app}"
Source: "..\licenses\GPL-3.0.txt"; DestDir: "{app}"
Source: "..\licenses\Apache-2.0.txt"; DestDir: "{app}"

[Icons]
Name: "{autoprograms}\ufsmount"; Filename: "{app}\ufsmount-tray.exe"; Comment: "Mount FreeBSD UFS disks read-only"

[Run]
Filename: "{app}\ufsmount-tray.exe"; Description: "Start ufsmount"; Flags: postinstall nowait skipifsilent runasoriginaluser

[Code]
const
  EnvKey = 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment';
  OwnKey = 'SOFTWARE\ufsmount';

var
  LauncherChecked: Boolean;
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

{ The WinFsp MSI next to the setup exe, if it is the expected one. }
function LocalMsiOk(const Msi: String): Boolean;
begin
  try
    Result := FileExists(Msi) and (GetSHA256OfFile(Msi) = '{#WinFspSha256}');
  except
    Result := False;
  end;
end;

{ Install WinFsp if it is missing or too old. Returns an error text, or ''.
  A restart request of the MSI (3010) is passed on through NeedRestart. }
function EnsureWinFsp: String;
var
  Msi, MsiLog, UI: String;
  Code: Integer;
begin
  Result := '';
  if WinFspOk then
    exit;
  Msi := ExpandConstant('{src}\{#WinFspMsi}');
  if not LocalMsiOk(Msi) then begin
    try
      DownloadTemporaryFile('{#WinFspUrl}', '{#WinFspMsi}', '{#WinFspSha256}', nil);
    except
      Result := 'WinFsp could not be downloaded: ' + GetExceptionMessage + #13#10#13#10 +
        'Install WinFsp {#WinFspMinMajor}.{#WinFspMinMinor}.{#WinFspMinBuild} or later from ' +
        'https://github.com/winfsp/winfsp/releases and run this setup again.';
      exit;
    end;
    Msi := ExpandConstant('{tmp}\{#WinFspMsi}');
  end;
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

{ --- PATH --- }

function SameDir(A, B: String): Boolean;
begin
  Result := CompareText(RemoveBackslashUnlessRoot(Trim(A)), RemoveBackslashUnlessRoot(B)) = 0;
end;

function PathHas(Paths, Dir: String): Boolean;
var
  P: Integer;
begin
  Result := False;
  while Paths <> '' do begin
    P := Pos(';', Paths);
    if P = 0 then begin
      Result := SameDir(Paths, Dir);
      exit;
    end;
    if SameDir(Copy(Paths, 1, P - 1), Dir) then begin
      Result := True;
      exit;
    end;
    Delete(Paths, 1, P);
  end;
end;

{ Append our folder to the system PATH unless it is there already (system or
  user PATH). Remember exactly what was appended, for the uninstaller. }
procedure AddToPath;
var
  Paths, UserPaths, Dir, Added: String;
begin
  Dir := ExpandConstant('{app}');
  if not RegQueryStringValue(HKLM, EnvKey, 'Path', Paths) then begin
    Log('PATH: cannot read it; not changed');
    exit;
  end;
  if PathHas(Paths, Dir) then
    exit;
  if RegQueryStringValue(HKCU, 'Environment', 'Path', UserPaths) and PathHas(UserPaths, Dir) then
    exit;
  if (Paths = '') or (Paths[Length(Paths)] = ';') then
    Added := Dir
  else
    Added := ';' + Dir;
  if RegWriteExpandStringValue(HKLM, EnvKey, 'Path', Paths + Added) then
    RegWriteStringValue(HKLM, OwnKey, 'PathAdded', Added)
  else
    Log('PATH: cannot write it');
end;

{ Paths without the entries for Dir; all other entries stay as they are. }
function WithoutDir(Paths, Dir: String): String;
var
  P: Integer;
  Part: String;
  First: Boolean;
begin
  Result := '';
  First := True;
  while True do begin
    P := Pos(';', Paths);
    if P = 0 then
      Part := Paths
    else
      Part := Copy(Paths, 1, P - 1);
    if not SameDir(Part, Dir) then begin
      if not First then
        Result := Result + ';';
      Result := Result + Part;
      First := False;
    end;
    if P = 0 then
      break;
    Delete(Paths, 1, P);
  end;
end;

procedure RemoveFromPath;
var
  Paths, Added: String;
  N: Integer;
begin
  if not RegQueryStringValue(HKLM, OwnKey, 'PathAdded', Added) then
    exit;
  if RegQueryStringValue(HKLM, EnvKey, 'Path', Paths) then begin
    N := Length(Paths) - Length(Added);
    if (N >= 0) and (Copy(Paths, N + 1, Length(Added)) = Added) then
      { Still at the end: take back exactly what was appended. }
      RegWriteExpandStringValue(HKLM, EnvKey, 'Path', Copy(Paths, 1, N))
    else
      RegWriteExpandStringValue(HKLM, EnvKey, 'Path', WithoutDir(Paths, ExpandConstant('{app}')));
  end;
  RegDeleteKeyIncludingSubkeys(HKLM, OwnKey);
end;

{ --- setup steps --- }

procedure CurPageChanged(CurPageID: Integer);
begin
  { Keep an existing launcher registration selected. }
  if (CurPageID = wpSelectTasks) and not LauncherChecked then begin
    LauncherChecked := True;
    if RegKeyExists(HKLM32, 'SOFTWARE\WinFsp\Services\ufs') then
      WizardSelectTasks('launcher');
  end;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  StopRunning;
  Result := EnsureWinFsp;
end;

function NeedRestart: Boolean;
begin
  Result := RestartNeeded;
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  Params: String;
begin
  if CurStep <> ssPostInstall then
    exit;
  Params := '--install-task';
  if WizardIsTaskSelected('logon') then
    Params := Params + ' --logon';
  RunOrWarn(ExpandConstant('{app}\ufsmount-tray.exe'), Params, 'creating the start task');
  if WizardIsTaskSelected('launcher') then
    RunOrWarn(ExpandConstant('{app}\ufsmount.exe'), 'register', 'registering with the WinFsp launcher');
  if WizardIsTaskSelected('path') then
    AddToPath;
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
  RemoveFromPath;
end;
