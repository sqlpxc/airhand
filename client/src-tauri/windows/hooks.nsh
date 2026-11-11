; Airhand 安装包的附加步骤：Tauri NSIS 模板的钩子（tauri.conf.json → bundle.windows.nsis.installerHooks）。
;
; * 安装：复制完文件后运行 `airhand.exe --install`：注册并启动系统服务 Airhand（被控端），
;   按 config.toml 设置开机自动启动；config.toml 不存在时新建；
; * 升级：复制文件之前运行旧版本的 `airhand.exe --stop-service`，停止服务、等托盘退出，否则 airhand.exe 正在使用，无法覆盖；
; * 卸载：运行 `airhand.exe --uninstall`：停止并删除服务、删除开机启动项、等托盘退出；
;   然后删除安装目录里程序运行时生成的文件（config.toml）、安装程序记下的注册表项、防火墙里为 airhand.exe 添加的规则。
;
; 升级时如果选择"先卸载旧版本"，旧版本的卸载程序由新的安装程序启动：安装程序启动时设置环境变量 AIRHAND_UPGRADE，
; 卸载程序继承了它，就保留 config.toml（控制码、服务器设置、控制台的"我的设备"）。直接卸载时没有这个变量，配置文件一起删除。
; 重新运行同一个版本的安装程序、在里面选"卸载 Airhand"是真的卸载：安装程序还把自己的版本放进 AIRHAND_INSTALLER_VERSION，
; 卸载程序看到和自己的版本一样，就照样全部删除。被动安装（/P）遇到同一个版本时会自动先卸载再装，那是重装，不传版本、保留配置。
; 删除配置文件时服务已经停了（PREUNINSTALL），服务里"配置文件被删掉就重新生成"的检查不会再把它写回来。

; 安装程序的界面初始化时（在"已安装旧版本"页面之前）调用
!define MUI_CUSTOMFUNCTION_GUIINIT AirhandGuiInit
Function AirhandGuiInit
  System::Call 'kernel32::SetEnvironmentVariableW(w "AIRHAND_UPGRADE", w "1")'
  Push $0
  ClearErrors
  ${GetOptions} $CMDLINE "/P" $0
  ${If} ${Errors}
    Call AirhandShareVersion
  ${EndIf}
  Pop $0
FunctionEnd

; 把本安装程序的版本放进环境变量 AIRHAND_INSTALLER_VERSION。版本取自安装程序版本信息里的 ProductVersion
; （Tauri 模板里写的就是 ${VERSION}）：这个文件比模板里的 !define VERSION 更早被引入，这里用不了 ${VERSION}，只能运行时读。
; 读不到时不设置，卸载程序就按升级处理（保留配置），和以前一样
Function AirhandShareVersion
  Push $0
  Push $1
  Push $2
  Push $3
  Push $4
  StrCpy $4 ""
  System::Call 'version::GetFileVersionInfoSizeW(w "$EXEPATH", p 0) i .r0'
  ${If} $0 > 0
    System::Alloc $0
    Pop $1
    System::Call 'version::GetFileVersionInfoW(w "$EXEPATH", i 0, i r0, p r1) i .r2'
    ${If} $2 <> 0
      ; 版本信息里的语言和代码页（NSIS 只写一组）
      System::Call 'version::VerQueryValueW(p r1, w "\VarFileInfo\Translation", *p .r2, *i .r3) i .r0'
      ${If} $0 <> 0
      ${AndIf} $3 >= 4
        System::Call '*$2(&i2 .r0, &i2 .r3)'
        IntFmt $0 "%04x" $0
        IntFmt $3 "%04x" $3
        System::Call 'version::VerQueryValueW(p r1, w "\StringFileInfo\$0$3\ProductVersion", *p .r2, *i .r3) i .r0'
        ${If} $0 <> 0
        ${AndIf} $3 > 0
          System::Call '*$2(&w$3 .r4)'
        ${EndIf}
      ${EndIf}
    ${EndIf}
    System::Free $1
  ${EndIf}
  ${If} $4 != ""
    System::Call 'kernel32::SetEnvironmentVariableW(w "AIRHAND_INSTALLER_VERSION", w r4)'
  ${EndIf}
  Pop $4
  Pop $3
  Pop $2
  Pop $1
  Pop $0
FunctionEnd

!macro NSIS_HOOK_PREINSTALL
  ${If} ${FileExists} "$INSTDIR\${MAINBINARYNAME}.exe"
    DetailPrint "Stopping the Airhand service…"
    nsExec::ExecToLog '"$INSTDIR\${MAINBINARYNAME}.exe" --stop-service'
    Pop $0
  ${EndIf}
!macroend

!macro NSIS_HOOK_POSTINSTALL
  DetailPrint "Installing the Airhand service…"
  nsExec::ExecToLog '"$INSTDIR\${MAINBINARYNAME}.exe" --install'
  Pop $0
  ${If} $0 != 0
    MessageBox MB_ICONEXCLAMATION "无法安装 Airhand 系统服务（$0），本机将不能被远程控制。$\r$\nCan't install the Airhand system service ($0), this computer can't be controlled remotely."
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  DetailPrint "Removing the Airhand service…"
  nsExec::ExecToLog '"$INSTDIR\${MAINBINARYNAME}.exe" --uninstall'
  Pop $0
  ${If} $0 != 0
    ; airhand.exe 运行失败（例如已经损坏）：用系统工具删除服务和开机启动项
    nsExec::ExecToLog '"$SYSDIR\sc.exe" stop Airhand'
    Pop $0
    Sleep 3000
    nsExec::ExecToLog '"$SYSDIR\sc.exe" delete Airhand'
    Pop $0
    DeleteRegValue HKLM "Software\Microsoft\Windows\CurrentVersion\Run" "Airhand"
  ${EndIf}
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  ReadEnvStr $0 AIRHAND_UPGRADE
  ; 同一个版本的安装程序里选"卸载 Airhand"：不是升级，是真的卸载
  ReadEnvStr $1 AIRHAND_INSTALLER_VERSION
  ${If} $1 == "${VERSION}"
    StrCpy $0 ""
  ${EndIf}
  ${If} $UpdateMode <> 1
  ${AndIf} $0 != "1"
    Delete "$INSTDIR\config.toml"
    Delete "$INSTDIR\config.toml.tmp"
    RMDir "$INSTDIR"
    ; 安装程序记下的安装位置和语言（Tauri 只在勾选"删除应用程序数据"时删除）
    DeleteRegKey SHCTX "${MANUPRODUCTKEY}"
    DeleteRegKey /ifempty SHCTX "${MANUKEY}"
    DeleteRegValue HKCU "${MANUPRODUCTKEY}" "Installer Language"
    DeleteRegKey /ifempty HKCU "${MANUPRODUCTKEY}"
    DeleteRegKey /ifempty HKCU "${MANUKEY}"
    ; 用户在 Windows 防火墙的提示里允许 airhand.exe 时系统添加的规则
    nsExec::ExecToLog '"$SYSDIR\netsh.exe" advfirewall firewall delete rule name=all program="$INSTDIR\${MAINBINARYNAME}.exe"'
    Pop $0
  ${EndIf}
  ; 控制台的 WebView 临时目录（正常退出时已经删除，这里清理强制结束时的残留）
  FindFirst $1 $2 "$TEMP\airhand-webview-*"
  ${DoWhile} $2 != ""
    RMDir /r "$TEMP\$2"
    FindNext $1 $2
  ${Loop}
  FindClose $1
!macroend
