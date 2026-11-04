#ifndef SourceDir
  #error SourceDir must point to the prepared Windows package
#endif
#ifndef AppVersion
  #error AppVersion must be provided by the build script
#endif

[Setup]
AppId=io.openmouse.bridge
AppName=OpenMouse Bridge
AppVersion={#AppVersion}
AppPublisher=OpenMouse Project
AppPublisherURL=https://openmouse.app/
DefaultDirName={localappdata}\Programs\OpenMouse Bridge
DefaultGroupName=OpenMouse Bridge
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
OutputBaseFilename=OpenMouse-Bridge-windows-x64-setup
SetupIconFile=..\..\assets\openmouse-windows.ico
UninstallDisplayIcon={app}\openmouse-bridge.exe
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
CloseApplications=yes
RestartApplications=no

[Tasks]
Name: desktopicon; Description: "Create a desktop shortcut"; Flags: unchecked

[Files]
Source: "{#SourceDir}\openmouse-bridge.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\DM-Sans-OFL.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\native-hid\*"; DestDir: "{app}\native-hid"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{autoprograms}\OpenMouse Bridge"; Filename: "{app}\openmouse-bridge.exe"
Name: "{autodesktop}\OpenMouse Bridge"; Filename: "{app}\openmouse-bridge.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\openmouse-bridge.exe"; Description: "Launch OpenMouse Bridge"; Flags: nowait postinstall skipifsilent
