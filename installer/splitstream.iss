; Splitstream installer (Inno Setup 6). Build with:
;   iscc installer\splitstream.iss
; from the repo root, after `cargo build --release -p app`.
;
; Machine-wide, elevated install (simple-launch.md L4 decision: Program
; Files, UAC on install, single copy for all users). Deliberately does NOT
; write per-user config or HKCU autostart here — the installer runs
; elevated (admin), but config lives in the *end user's* %APPDATA% and
; autostart in the *end user's* HKCU Run key, so the app's own first-run
; (launched `runasoriginaluser`, never the elevated installer) owns both.
;
; Ships unsigned for v1 (Splitstream-Engineering-Spec.md override log,
; 2026-07-20, user confirmed) — an OV/EV Authenticode cert's cost/renewal
; is the same class of overhead that permanently killed the P6 own-driver
; for a free, no-revenue OSS project. Expect SmartScreen's "Windows
; protected your PC" on first download/run; document the "More info -> Run
; anyway" step in the README, not solved here.

#define MyAppName "Splitstream"
#define MyAppVersion "0.1.0"
#define MyAppExeName "splitstream.exe"
#define MyAppPublisher "Splitstream"

[Setup]
AppId={{B8D6F2A4-6E3C-4B7A-9E3E-6F2B7C4A1D9C}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
DefaultDirName={autopf}\{#MyAppName}
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
PrivilegesRequired=admin
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\target\installer
OutputBaseFilename=SplitstreamSetup
Compression=lzma
SolidCompression=yes
UninstallDisplayIcon={app}\{#MyAppExeName}
WizardStyle=modern
; No SetupIconFile / app icon override — no .ico asset exists yet
; (simple-launch.md, dropped from scope 2026-07-20). Uses Inno's default
; wizard icon and the exe's own (currently default Rust) icon. Add both
; once real art exists.

[Files]
Source: "..\target\release\{#MyAppExeName}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"
Name: "{autoprograms}\Uninstall {#MyAppName}"; Filename: "{uninstallexe}"

[Run]
; `runasoriginaluser`: launched as the real end user, not the elevated
; installer — first-run config bootstrap + HKCU autostart registration
; must land in that user's hive (simple-launch.md Flow 1/2).
Filename: "{app}\{#MyAppExeName}"; Description: "Launch {#MyAppName}"; Flags: nowait postinstall skipifsilent runasoriginaluser

[UninstallRun]
; Deregisters the HKCU autostart entry before Program Files is removed.
; %APPDATA% config is deliberately left behind (Flow 6 decision — user
; data/setup survives a reinstall).
;
; `runasoriginaluser` (used in [Run] above) does not exist for
; [UninstallRun] — confirmed against Inno Setup docs, not assumed: once
; elevated, Windows gives Setup/Uninstall no way to recover the original
; pre-elevation user's credentials, a platform limitation Inno's own docs
; state outright, not an Inno gap. `runascurrentuser` (Uninstall's own,
; already-elevated credentials) is the closest available — correct in the
; common case (an admin user elevating via their own UAC consent prompt:
; same user, same HKCU hive, elevated or not). It under-cleans only if a
; *standard* user elevated Uninstall with *different* admin credentials —
; HKCU then resolves to the admin's hive, not the actual end user's,
; leaving their Run key dangling. Same class of accepted platform-limit
; tradeoff as shipping unsigned (see header comment); not solvable within
; Inno's flag system.
Filename: "{app}\{#MyAppExeName}"; Parameters: "--uninstall-cleanup"; RunOnceId: "DeregisterAutostart"; Flags: runascurrentuser
