; Inno Setup 6.3+ script for the PcVolumeControl Windows installer.
;
;   iscc /DAppVersion=1.2.3 packaging\windows\pcvolumecontrol.iss
;
; AppVersion must match the Cargo package version the exe was built with.
; BinDir (default target\release) holds pcvolumecontrol.exe; OutputDir
; (default target\installer) receives PcVolumeControl-<version>-setup.exe.
; Relative paths resolve against this script's directory.

#ifndef AppVersion
  #error Pass the version: iscc /DAppVersion=x.y.z
#endif
#ifndef BinDir
  #define BinDir "..\..\target\release"
#endif
#ifndef OutputDir
  #define OutputDir "..\..\target\installer"
#endif

; VersionInfoVersion takes only dotted numbers, so drop SemVer build metadata
; and any pre-release suffix.
#if Pos("+", AppVersion) > 0
  #define ReleaseVersion Copy(AppVersion, 1, Pos("+", AppVersion) - 1)
#else
  #define ReleaseVersion AppVersion
#endif
#if Pos("-", ReleaseVersion) > 0
  #define NumericVersion Copy(ReleaseVersion, 1, Pos("-", ReleaseVersion) - 1)
#else
  #define NumericVersion ReleaseVersion
#endif

#define AppName "PcVolumeControl"
#define AppExe "pcvolumecontrol.exe"
; The app manages this HKCU Run value itself (Settings, start on login).
#define RunValue "PcVolumeControl"

[Setup]
; Never change AppId: upgrades and uninstall find earlier installs by it.
AppId={{63ED99E8-31F1-4FA3-A5B5-DDAAF2F7463A}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=PcVolumeControl
AppPublisherURL=https://github.com/PcVolumeControl
AppSupportURL=https://github.com/PcVolumeControl/PcVolumeControlServer/issues
AppUpdatesURL=https://github.com/PcVolumeControl/PcVolumeControlServer/releases
VersionInfoVersion={#NumericVersion}
; Per-user install into %LOCALAPPDATA%\Programs by default; the user can pick
; an all-users install in Program Files from a dialog.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
DefaultDirName={autopf}\{#AppName}
DisableProgramGroupPage=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
SetupIconFile=..\..\crates\pcvc-app\assets\app.ico
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}
OutputDir={#OutputDir}
OutputBaseFilename={#AppName}-{#AppVersion}-setup
WizardStyle=modern
Compression=lzma2
SolidCompression=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Files]
Source: "{#BinDir}\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"; Parameters: "--show"

[Run]
Filename: "{app}\{#AppExe}"; Parameters: "--show"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall skipifsilent

[Code]
const
  RunKey = 'Software\Microsoft\Windows\CurrentVersion\Run';
  ApprovedKey = 'Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run';

var
  WasRunning: Boolean;

{ Stops copies running from this installation and waits for them to exit.
  Closing the window only hides the app to the tray, so a close request would
  not stop it. Returns whether any copy was running. }
function StopInstalledApp(): Boolean;
var
  Exe, Script: String;
  ResultCode: Integer;
begin
  Exe := ExpandConstant('{app}\{#AppExe}');
  StringChangeEx(Exe, '''', '''''', True);
  Script :=
    '$p = Get-Process -Name pcvolumecontrol -ErrorAction SilentlyContinue | ' +
    'Where-Object { $_.Path -eq ''' + Exe + ''' }; ' +
    'if (-not $p) { exit 1 }; ' +
    '$p | Stop-Process -Force; ' +
    '$p | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue; exit 0';
  Result := Exec(ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'),
    '-NoProfile -NonInteractive -Command "' + Script + '"', '', SW_HIDE,
    ewWaitUntilTerminated, ResultCode) and (ResultCode = 0);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  WasRunning := StopInstalledApp();
  Result := '';
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  ResultCode: Integer;
begin
  { Interactive installs offer to launch on the last page instead. }
  if (CurStep = ssPostInstall) and WasRunning and WizardSilent() then
    ExecAsOriginalUser(ExpandConstant('{app}\{#AppExe}'), '', '', SW_SHOWNORMAL,
      ewNoWait, ResultCode);
end;

{ Removes the start-on-login entry when it launches this installation. Only
  the uninstalling account's HKCU is reachable, so other accounts of an
  all-users install keep a stale entry. }
procedure RemoveRunValue();
var
  Command: String;
begin
  if RegQueryStringValue(HKCU, RunKey, '{#RunValue}', Command) then
    if Pos(Lowercase(ExpandConstant('{app}\{#AppExe}')), Lowercase(Command)) > 0 then
    begin
      RegDeleteValue(HKCU, RunKey, '{#RunValue}');
      RegDeleteValue(HKCU, ApprovedKey, '{#RunValue}');
    end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then
  begin
    StopInstalledApp();
    RemoveRunValue();
  end;
end;
