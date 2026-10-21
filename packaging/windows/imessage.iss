; Inno Setup 6 script for "imsg for Windows" (imessaged + imsg + imsg-tui).
; Built by tools/windows/build-installer.sh, which stages the files and passes:
;   /DAppVersion=0.1.13  /DStageDir=<dir with exes, LICENSE.txt, README-Windows.txt>
;   /DOutputDir=<dir>    and optionally /DTaskName=, /DServiceInstallArgs=,
;   /DServiceUninstallArgs=
;
; Product name: "imsg for Windows" -- named after the CLI, deliberately not
; "iMessage ..." so nothing suggests an Apple product.
;
; Per-user install (no admin): {autopf} is %LOCALAPPDATA%\Programs here.

#ifndef AppVersion
  #error Pass /DAppVersion=x.y.z (build-installer.sh does)
#endif
#ifndef StageDir
  #error Pass /DStageDir=<staging dir> (build-installer.sh does)
#endif
#ifndef OutputDir
  #define OutputDir "."
#endif
; Logon task that starts imessaged.
#ifndef TaskName
  #define TaskName "imessaged"
#endif
; The task is created (and started) by `imsg <ServiceInstallArgs>` and
; removed by `imsg <ServiceUninstallArgs>`; the script only uses schtasks to
; stop it before files are replaced and to make sure it is gone on uninstall.
#ifndef ServiceInstallArgs
  #define ServiceInstallArgs "service install"
#endif
#ifndef ServiceUninstallArgs
  #define ServiceUninstallArgs "service uninstall"
#endif

#define AppName "imsg for Windows"

[Setup]
AppId={{5722EC0A-15F7-46F0-BE22-1BAD63DAB43E}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=imessage-linux project
AppPublisherURL=https://github.com/thescruggs/imessage-linux
AppSupportURL=https://github.com/thescruggs/imessage-linux/issues
AppComments=Unofficial iMessage client engine and terminal clients. Not affiliated with Apple.
VersionInfoVersion={#AppVersion}
VersionInfoProductName={#AppName}
DefaultDirName={autopf}\imsg
DefaultGroupName=imsg
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputDir={#OutputDir}
OutputBaseFilename=imessage-{#AppVersion}-setup
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
LicenseFile={#StageDir}\LICENSE.txt
InfoAfterFile={#StageDir}\README-Windows.txt
; Broadcast WM_SETTINGCHANGE after the PATH edit (install and uninstall).
ChangesEnvironment=yes
CloseApplications=yes
RestartApplications=no
UninstallDisplayIcon={app}\imsg.exe
UninstallDisplayName={#AppName}
SetupLogging=yes

[Files]
Source: "{#StageDir}\imessaged.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\imsg.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\imsg-tui.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\LICENSE.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\README-Windows.txt"; DestDir: "{app}"; Flags: ignoreversion isreadme

[Icons]
; Windows Terminal if installed (its app-execution alias), else the exe
; directly (conhost, or whatever the default terminal is on Windows 11).
Name: "{group}\imsg TUI"; Filename: "{code:TuiFilename}"; Parameters: "{code:TuiParams}"; WorkingDir: "{%USERPROFILE}"; IconFilename: "{app}\imsg-tui.exe"; Comment: "imsg terminal UI"; AppUserModelID: "imessage-linux.imsg"
Name: "{group}\imsg doctor"; Filename: "{cmd}"; Parameters: "/k """"{app}\imsg.exe"" doctor"""; WorkingDir: "{%USERPROFILE}"; IconFilename: "{app}\imsg.exe"; Comment: "Check the imsg setup"
Name: "{group}\imsg first-run steps"; Filename: "{app}\README-Windows.txt"
Name: "{group}\Uninstall {#AppName}"; Filename: "{uninstallexe}"

[Run]
Filename: "{cmd}"; Parameters: "/k """"{app}\imsg.exe"" doctor"""; WorkingDir: "{%USERPROFILE}"; Description: "Run imsg doctor (checks the setup)"; Flags: postinstall nowait skipifsilent

[Code]
const
  TaskName = '{#TaskName}';
  EnvKey = 'Environment';

function WtPath(): String;
begin
  Result := ExpandConstant('{localappdata}\Microsoft\WindowsApps\wt.exe');
end;

function TuiFilename(Param: String): String;
begin
  if FileExists(WtPath()) then
    Result := WtPath()
  else
    Result := ExpandConstant('{app}\imsg-tui.exe');
end;

function TuiParams(Param: String): String;
begin
  if FileExists(WtPath()) then
    Result := '--title imsg "' + ExpandConstant('{app}\imsg-tui.exe') + '"'
  else
    Result := '';
end;

{ ---- user PATH (HKCU\Environment\Path) ---- }

function PathHasDir(Path, Dir: String): Boolean;
begin
  Result := Pos(';' + Uppercase(RemoveBackslashUnlessRoot(Dir)) + ';',
                ';' + Uppercase(Path) + ';') > 0;
end;

procedure AddToUserPath(Dir: String);
var
  Path: String;
begin
  if not RegQueryStringValue(HKCU, EnvKey, 'Path', Path) then
    Path := '';
  if PathHasDir(Path, Dir) then
    exit;
  if (Path <> '') and (Copy(Path, Length(Path), 1) <> ';') then
    Path := Path + ';';
  Path := Path + Dir;
  if not RegWriteExpandStringValue(HKCU, EnvKey, 'Path', Path) then
    Log('could not update the user PATH')
  else
    Log('added to user PATH: ' + Dir);
end;

procedure RemoveFromUserPath(Dir: String);
var
  Path, Rest, Item, Kept: String;
  P: Integer;
begin
  if not RegQueryStringValue(HKCU, EnvKey, 'Path', Path) then
    exit;
  Rest := Path;
  Kept := '';
  while Rest <> '' do
  begin
    P := Pos(';', Rest);
    if P = 0 then
    begin
      Item := Rest;
      Rest := '';
    end else begin
      Item := Copy(Rest, 1, P - 1);
      Rest := Copy(Rest, P + 1, Length(Rest));
    end;
    if (Item <> '') and
       (CompareText(RemoveBackslashUnlessRoot(Item), RemoveBackslashUnlessRoot(Dir)) <> 0) then
    begin
      if Kept <> '' then
        Kept := Kept + ';';
      Kept := Kept + Item;
    end;
  end;
  if Kept <> Path then
  begin
    if Kept = '' then
      RegDeleteValue(HKCU, EnvKey, 'Path')
    else
      RegWriteExpandStringValue(HKCU, EnvKey, 'Path', Kept);
    Log('removed from user PATH: ' + Dir);
  end;
end;

{ ---- logon task ---- }

function RunHidden(Exe, Params: String): Integer;
var
  Code: Integer;
begin
  if Exec(Exe, Params, '', SW_HIDE, ewWaitUntilTerminated, Code) then
    Result := Code
  else
    Result := -1;
  Log(Format('%s %s -> %d', [Exe, Params, Result]));
end;

function Schtasks(Params: String): Integer;
begin
  Result := RunHidden(ExpandConstant('{sys}\schtasks.exe'), Params);
end;

{ `imsg service install` creates the per-user logon task and starts it. }
function CreateLogonTask(): Boolean;
begin
  Result := RunHidden(ExpandConstant('{app}\imsg.exe'), '{#ServiceInstallArgs}') = 0;
end;

{ Stop the daemon (task and any hand-started copy) so its exe can be replaced
  or removed. }
procedure StopDaemon();
begin
  { Graceful first: `imsg service stop` asks the daemon to save its push/IDS
    state and exit (waits up to 15 s). The kill below only catches leftovers. }
  if FileExists(ExpandConstant('{app}\imsg.exe')) then
    RunHidden(ExpandConstant('{app}\imsg.exe'), 'service stop');
  Schtasks('/End /TN "' + TaskName + '"');
  RunHidden(ExpandConstant('{sys}\taskkill.exe'), '/F /IM imessaged.exe');
end;

procedure DeleteLogonTask();
begin
  if FileExists(ExpandConstant('{app}\imsg.exe')) then
    RunHidden(ExpandConstant('{app}\imsg.exe'), '{#ServiceUninstallArgs}');
  { Also when imsg did it: harmless if the task is gone. }
  Schtasks('/Delete /F /TN "' + TaskName + '"');
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  StopDaemon();
  Result := '';
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
  begin
    AddToUserPath(ExpandConstant('{app}'));
    if not CreateLogonTask() then
      SuppressibleMsgBox(
        'Could not create the logon task "' + TaskName + '" that starts imessaged.' + #13#10 +
        'Run "imsg service install" in a terminal to see why; README-Windows.txt has the details.',
        mbError, MB_OK, IDOK);
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then
  begin
    StopDaemon();
    DeleteLogonTask();
    RemoveFromUserPath(ExpandConstant('{app}'));
  end;
end;
