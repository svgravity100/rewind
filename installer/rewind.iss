; Installer for Rewind (Inno Setup 6). Built by scripts/package.ps1:
;   ISCC /DAppVersion=0.1.0 /DSourceExe=...\Rewind.exe /DOutputDir=...\dist installer\rewind.iss
; Installs for the current user only (no administrator rights needed).

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef SourceExe
  #define SourceExe "..\app\target\release\rewind.exe"
#endif
#ifndef OutputDir
  #define OutputDir "..\dist"
#endif

#ifndef IconFile
  #define IconFile "app.ico"
#endif

#define AppName "Rewind"
#define AppExe "Rewind.exe"

[Setup]
AppId={{8E3C2F7A-5B1D-4C9E-A6F0-2D7B9E4C1A35}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppName}
DefaultDirName={localappdata}\Programs\Rewind
; 0.1.0–0.2.0 installed to "ProgramsRewind" by mistake: always use the right folder.
UsePreviousAppDir=no
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir={#OutputDir}
OutputBaseFilename=Rewind-{#AppVersion}-setup
SetupIconFile={#IconFile}
UninstallDisplayIcon={app}\{#AppExe}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
; The program holds this mutex while running: setup asks to close it first.
AppMutex=RewindReplayBuffer

[Languages]
Name: "ru"; MessagesFile: "compiler:Languages\Russian.isl"
Name: "en"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
Name: "autostart"; Description: "Запускать вместе с Windows (сразу в трей)"; Flags: unchecked

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; DestName: "{#AppExe}"; Flags: ignoreversion

[InstallDelete]
; The wrongly named folder of 0.1.0–0.2.0 (the program there is closed before setup runs).
Type: filesandordirs; Name: "{localappdata}\ProgramsRewind"

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{group}\{cm:UninstallProgram,{#AppName}}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Registry]
; Same value the program's own "Запуск с Windows" switch writes.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "Rewind"; ValueData: """{app}\{#AppExe}"" --tray"; Tasks: autostart
; Removed on uninstall whether it was set here or in the program.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: none; ValueName: "Rewind"; Flags: uninsdeletevalue dontcreatekey

[Run]
Filename: "{app}\{#AppExe}"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall skipifsilent
; After a silent update (started by the program itself): start the new version.
Filename: "{app}\{#AppExe}"; Flags: nowait postinstall skipifnotsilent
