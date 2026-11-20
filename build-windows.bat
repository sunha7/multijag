@echo off
REM One-click build for MultiJag on Windows. Run from the project folder.
where node >nul 2>nul || (echo Node.js 20+ is required: https://nodejs.org & pause & exit /b 1)
where cargo >nul 2>nul || (echo Rust is required: https://rustup.rs & pause & exit /b 1)
REM Keep your username and folder paths out of the compiled exe (Rust embeds source paths).
if not exist src-tauri\.cargo mkdir src-tauri\.cargo
echo [target.'cfg(all(windows, target_env = "msvc"))']> src-tauri\.cargo\config.toml
set "FLAGS='--remap-path-prefix=%USERPROFILE%=~', '--remap-path-prefix=%CD%=.', '-Clink-arg=/PDBALTPATH:multijag.pdb'"
if defined CARGO_HOME set "FLAGS=%FLAGS%, '--remap-path-prefix=%CARGO_HOME%=~/.cargo'"
if defined RUSTUP_HOME set "FLAGS=%FLAGS%, '--remap-path-prefix=%RUSTUP_HOME%=~/.rustup'"
echo rustflags = [%FLAGS%]>> src-tauri\.cargo\config.toml
call npm install || (pause & exit /b 1)
call npm run build || (pause & exit /b 1)
echo.
echo Done. Exe:        src-tauri\target\release\multijag.exe
echo       Installers: src-tauri\target\release\bundle\
pause
