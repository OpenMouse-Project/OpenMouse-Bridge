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
MinVersion=10.0
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

[Code]
var
  DeleteUserData: Boolean;
  ExistingPage: TInputOptionWizardPage;
  UninstallCommand: String;
  InstalledVersion: String;
  LeaveSetup: Boolean;

function FindInstalled(): Boolean;
var
  Key: String;
begin
  Key := 'Software\Microsoft\Windows\CurrentVersion\Uninstall\io.openmouse.bridge_is1';
  Result := RegQueryStringValue(HKCU, Key, 'UninstallString', UninstallCommand);
  if Result then
  begin
    if not RegQueryStringValue(HKCU, Key, 'DisplayVersion', InstalledVersion) then
      InstalledVersion := '';
  end;
end;

procedure InitializeWizard();
var
  Choice: String;
begin
  if FindInstalled() then
  begin
    if InstalledVersion = '{#AppVersion}' then
      Choice := 'Repair Bridge (install the current version again)'
    else
      Choice := 'Update Bridge to {#AppVersion}';
    ExistingPage := CreateInputOptionPage(wpWelcome,
      'OpenMouse Bridge is already installed',
      'Installed version: ' + InstalledVersion,
      'What would you like to do?', True, False);
    ExistingPage.Add(Choice);
    ExistingPage.Add('Uninstall Bridge');
    ExistingPage.Values[0] := True;
  end;
end;

function NextButtonClick(CurPageID: Integer): Boolean;
var
  ResultCode: Integer;
begin
  Result := True;
  if (ExistingPage <> nil) and (CurPageID = ExistingPage.ID) and ExistingPage.Values[1] then
  begin
    Exec(RemoveQuotes(UninstallCommand), '', '', SW_SHOWNORMAL, ewNoWait, ResultCode);
    LeaveSetup := True;
    WizardForm.Close;
    Result := False;
  end;
end;

procedure CancelButtonClick(CurPageID: Integer; var Cancel, Confirm: Boolean);
begin
  if LeaveSetup then
    Confirm := False;
end;

function BridgeProcesses(StopThem: Boolean): Integer;
var
  Script, Flag: String;
  ResultCode: Integer;
begin
  Result := 0;
  Script :=
    'param([string]$Dir, [string]$Stop)' + #13#10 +
    '$found = @(Get-CimInstance Win32_Process | Where-Object { $_.Name -eq "openmouse-bridge.exe" -and $_.ExecutablePath -and $_.ExecutablePath.StartsWith($Dir, [StringComparison]::OrdinalIgnoreCase) })' + #13#10 +
    'if ($Stop -eq "1") { $found | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }; Start-Sleep -Seconds 1 }' + #13#10 +
    'exit $found.Count';
  if StopThem then
    Flag := '1'
  else
    Flag := '0';
  if SaveStringToFile(ExpandConstant('{tmp}\check-bridge.ps1'), Script, False) then
    if Exec(ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'),
      '-NoProfile -ExecutionPolicy Bypass -File "' + ExpandConstant('{tmp}\check-bridge.ps1') + '" -Dir "' + ExpandConstant('{app}') + '" -Stop ' + Flag,
      '', SW_HIDE, ewWaitUntilTerminated, ResultCode) then
      Result := ResultCode;
end;

function InitializeUninstall(): Boolean;
begin
  Result := True;
  if BridgeProcesses(False) > 0 then
  begin
    if SuppressibleMsgBox('OpenMouse Bridge is running.' + #13#10 + #13#10 + 'Close it so it can be uninstalled?', mbConfirmation, MB_YESNO, IDYES) = IDYES then
      BridgeProcesses(True)
    else
      Result := False;
  end;
  if Result then
    DeleteUserData := SuppressibleMsgBox(
      'Also delete your Bridge settings, profiles and logs?' + #13#10 + #13#10 +
      'Choose No to keep them for a later reinstall.',
      mbConfirmation, MB_YESNO, IDNO) = IDYES;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  ResultCode: Integer;
begin
  if CurUninstallStep = usUninstall then
  begin
    BridgeProcesses(True);
    Exit;
  end;
  if CurUninstallStep <> usPostUninstall then
    Exit;
  if DeleteUserData then
  begin
    DelTree(ExpandConstant('{userappdata}\OpenMouse\OpenMouse Bridge'), True, True, True);
    RemoveDir(ExpandConstant('{userappdata}\OpenMouse'));
  end;
  Exec(ExpandConstant('{cmd}'), '/C ping -n 8 127.0.0.1 >nul & rmdir "' + ExpandConstant('{app}') + '"', '', SW_HIDE, ewNoWait, ResultCode);
end;
