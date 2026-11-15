; Inno Setup 6.3+ script for the PcVolumeControl Windows installer.
;
;   iscc /DAppVersion=1.2.3 packaging\windows\pcvolumecontrol.iss
;
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

; VersionInfoVersion takes only dotted numbers, so drop any pre-release suffix.
#if Pos("-", AppVersion) > 0
  #define NumericVersion Copy(AppVersion, 1, Pos("-", AppVersion) - 1)
#else
  #define NumericVersion AppVersion
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
SetupIconFile=pcvolumecontrol.ico
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

[UninstallRun]
; Closing the window only hides it to the tray, so the app is stopped with
; taskkill rather than a close request.
Filename: "{sys}\taskkill.exe"; Parameters: "/F /IM {#AppExe}"; Flags: runhidden; RunOnceId: "StopApp"

[Code]
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  ResultCode: Integer;
begin
  { Stop a running copy so its executable can be replaced. }
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM {#AppExe}', '', SW_HIDE,
    ewWaitUntilTerminated, ResultCode);
  Result := '';
end;

const
  RunKey = 'Software\Microsoft\Windows\CurrentVersion\Run';
  ApprovedKey = 'Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run';

{ Removes the start-on-login entry, but only when it launches this
  installation. }
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Command: String;
begin
  if CurUninstallStep = usUninstall then
    if RegQueryStringValue(HKCU, RunKey, '{#RunValue}', Command) then
      if Pos(Lowercase(ExpandConstant('{app}\{#AppExe}')), Lowercase(Command)) > 0 then
      begin
        RegDeleteValue(HKCU, RunKey, '{#RunValue}');
        RegDeleteValue(HKCU, ApprovedKey, '{#RunValue}');
      end;
end;
