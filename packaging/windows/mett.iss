; mett's Windows installer, built by packaging/windows-installer.sh, which
; defines Version, NumericVersion (Version without a pre-release part) and
; Source (the directory with mett.exe and its files).
;
; It installs for the user (no administrator needed), or for everyone when
; run as administrator, and adds the install directory to that user's or
; the system's PATH; uninstalling takes it out again.

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
UninstallDisplayIcon={app}\mett.exe
UninstallDisplayName=mett

[Tasks]
Name: addtopath; Description: "Add mett to the PATH, so it runs from any terminal"

[Files]
Source: "{#Source}\*"; DestDir: "{app}"; Flags: ignoreversion

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
  RegWriteExpandStringValue(Root, Key, 'Path', Path + Dir);
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
  if CurUninstallStep = usPostUninstall then
    RemoveFromPath(ExpandConstant('{app}'));
end;
