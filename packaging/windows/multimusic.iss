; Inno Setup script for the Windows installer.
; Built by .github/workflows/release.yml:  ISCC /DAppVersion=0.2.0 /DStage=<folder> multimusic.iss
; <Stage> holds multimusic.exe, tools\ (mpv, yt-dlp, ffmpeg, ffprobe), fonts\ and the license.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef Stage
  #define Stage "..\..\stage"
#endif

[Setup]
AppId={{6B0E8D4A-1F57-4C6B-9E1B-2D7A5C1F3E90}
AppName=MultiMusic
AppVersion={#AppVersion}
AppVerName=MultiMusic {#AppVersion}
AppPublisher=v0-0x
AppPublisherURL=https://github.com/v0-0x/localmusicplayer
AppSupportURL=https://github.com/v0-0x/localmusicplayer/issues
AppUpdatesURL=https://github.com/v0-0x/localmusicplayer/releases
; Installed for the current user: no administrator prompt, and updates install silently.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
DefaultDirName={autopf}\MultiMusic
DefaultGroupName=MultiMusic
DisableProgramGroupPage=yes
DisableDirPage=auto
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputBaseFilename=MultiMusic-Setup-{#AppVersion}-x64
OutputDir=..\..\dist
SetupIconFile=..\..\assets\multimusic.ico
UninstallDisplayIcon={app}\multimusic.exe
UninstallDisplayName=MultiMusic
LicenseFile={#Stage}\LICENSE.txt
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
; A running MultiMusic is closed for the update (it starts again afterwards).
CloseApplications=force
RestartApplications=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"

[InstallDelete]
; Tools from older versions are replaced as a whole.
Type: filesandordirs; Name: "{app}\tools"

[Files]
Source: "{#Stage}\multimusic.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Stage}\tools\*"; DestDir: "{app}\tools"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "{#Stage}\fonts\*"; DestDir: "{app}\fonts"; Flags: ignoreversion
Source: "{#Stage}\LICENSE.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Stage}\THIRD-PARTY.txt"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\MultiMusic"; Filename: "{app}\multimusic.exe"
Name: "{autodesktop}\MultiMusic"; Filename: "{app}\multimusic.exe"; Tasks: desktopicon

[Run]
; Offered after a normal install; run right away after a silent update.
Filename: "{app}\multimusic.exe"; Description: "{cm:LaunchProgram,MultiMusic}"; Flags: nowait postinstall

[UninstallDelete]
Type: filesandordirs; Name: "{app}\tools"
; Your settings, library and logins (in %APPDATA%\multimusic) are kept.
