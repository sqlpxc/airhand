// 网页版用不了时的说明页。
//
// 浏览器只在安全的页面（HTTPS 或 localhost）上提供 WebCodecs、AudioWorklet、剪贴板这些功能；
// 画面在 Worker 里解码、绘制，还要 OffscreenCanvas。
// airhand-web 在同一个端口上同时支持 HTTP 和 HTTPS，所以只要把地址换成 https:// 就行。
import { api } from "../lib/api";
import { useT } from "../lib/i18n";
import { LangSwitch } from "./AppMenu";
import { IconWarn } from "./Icons";

export type Unsupported = "insecure" | "noWebCodecs";

export function webUnsupported(): Unsupported | null {
  if (api.kind !== "web") return null;
  if (!window.isSecureContext) return "insecure";
  if (typeof VideoDecoder === "undefined" || typeof OffscreenCanvas === "undefined") return "noWebCodecs";
  return null;
}

export function WebUnsupported({ reason }: { reason: Unsupported }) {
  const t = useT();
  const https = new URL(location.href);
  https.protocol = "https:";
  return (
    <div className="app">
      <main className="main">
        <div className="newtab">
          <div className="nt-wrap">
            <section className="nt-card">
              <div className="nt-head">
                <div className="hero-icon">
                  <IconWarn size={26} />
                </div>
                {reason === "insecure" ? (
                  <>
                    <h1>{t("web.insecure.title")}</h1>
                    <p className="muted">{t("web.insecure.body")}</p>
                  </>
                ) : (
                  <>
                    <h1>{t("web.unsupported.title")}</h1>
                    <p className="muted">{t("web.unsupported.body")}</p>
                  </>
                )}
              </div>
              {reason === "insecure" && (
                <>
                  <a className="primary-btn link-btn" href={https.href}>
                    {https.href}
                  </a>
                  <p className="muted small center">{t("web.insecure.cert")}</p>
                </>
              )}
              <div className="center">
                <LangSwitch />
              </div>
            </section>
          </div>
        </div>
      </main>
    </div>
  );
}
