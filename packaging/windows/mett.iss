; mett's Windows installer, built by packaging/windows-installer.sh, which
; defines Version, NumericVersion (Version without a pre-release part) and
; Source (the directory with mett.exe, mett-gui.exe and their files).
;
; Start's menu gets a shortcut to mett-gui, mett in a window of its own
; (and the desktop one, if asked for).
;
; It installs for the user (no administrator needed), or for everyone when
; run as administrator, and adds the install directory to that user's or
; the system's PATH; uninstalling takes it out again, if it put it there
; (as the file path-added in the install directory says).

[Setup]
; Never change: Windows tells installs of mett apart from other programs by it.
AppId={{522A6D2E-88BC-45A5-AA93-88B3214C345C}
AppName=mett
AppVersion={#Version}
AppVerName=mett {#Version}
AppPublisher=Isaac Gelado
AppPublisherURL=https://github.com/igelado/mett
AppSupportURL=https://github.com/igelado/mett/issues
VersionInfoVersion={#NumericVersion}
DefaultDirName={autopf}\mett
DisableProgramGroupPage=yes
DisableDirPage=auto
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ChangesEnvironment=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
UninstallDisplayIcon={app}\mett.ico
UninstallDisplayName=mett

[Tasks]
Name: addtopath; Description: "Add mett to the PATH, so it runs from any terminal"
Name: desktopicon; Description: "A shortcut to mett on the desktop"; Flags: unchecked

[Icons]
Name: "{autoprograms}\mett"; Filename: "{app}\mett-gui.exe"; IconFilename: "{app}\mett.ico"; Comment: "Office365 mail, as mutt reads it"
Name: "{autodesktop}\mett"; Filename: "{app}\mett-gui.exe"; IconFilename: "{app}\mett.ico"; Tasks: desktopicon

[Files]
Source: "{#Source}\*"; DestDir: "{app}"; Flags: ignoreversion

[UninstallDelete]
Type: files; Name: "{app}\path-added"

[Code]
{ The environment's registry key: the system's when installing for
  everyone, else the user's. }
function EnvironmentKey(var Root: Integer): String;
begin
  if IsAdminInstallMode then begin
    Root := HKEY_LOCAL_MACHINE;
    Result := 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment';
  end else begin
    Root := HKEY_CURRENT_USER;
    Result := 'Environment';
  end;
end;

{ Adds Dir to the PATH, unless it is there already; and if it added it,
  says so in Dir\path-added, for uninstalling. }
procedure AddToPath(Dir: String);
var
  Root: Integer;
  Key, Path: String;
begin
  Key := EnvironmentKey(Root);
  if not RegQueryStringValue(Root, Key, 'Path', Path) then
    Path := '';
  if Pos(';' + Uppercase(Dir) + ';', ';' + Uppercase(Path) + ';') > 0 then
    exit;
  if (Path <> '') and (Path[Length(Path)] <> ';') then
    Path := Path + ';';
  if RegWriteExpandStringValue(Root, Key, 'Path', Path + Dir) then
    SaveStringToFile(Dir + '\path-added', '', False);
end;

procedure RemoveFromPath(Dir: String);
var
  Root, At: Integer;
  Key, Path: String;
begin
  Key := EnvironmentKey(Root);
  if not RegQueryStringValue(Root, Key, 'Path', Path) then
    exit;
  Path := ';' + Path + ';';
  At := Pos(';' + Uppercase(Dir) + ';', Uppercase(Path));
  if At = 0 then
    exit;
  Delete(Path, At, Length(Dir) + 1);
  RegWriteExpandStringValue(Root, Key, 'Path', Copy(Path, 2, Length(Path) - 2));
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if (CurStep = ssPostInstall) and WizardIsTaskSelected('addtopath') then
    AddToPath(ExpandConstant('{app}'));
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  { Before the files go, path-added among them. }
  if (CurUninstallStep = usUninstall) and FileExists(ExpandConstant('{app}\path-added')) then
    RemoveFromPath(ExpandConstant('{app}'));
end;
