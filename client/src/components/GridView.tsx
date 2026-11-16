import { closeTab, fmtCode } from "../lib/actions";
import { useT } from "../lib/i18n";
import { sessionStateText } from "../lib/messages";
import type { RemoteSession } from "../lib/session";
import { store, useSession, useSessionTitle, useStore } from "../lib/store";
import { IconClose, IconPlus } from "./Icons";
import { CanvasHost } from "./SessionView";

/** 多屏总览：所有会话的实时画面，点击进入控制 */
export function GridView() {
  const t = useT();
  const sessions = useStore((s) => s.sessions);
  const n = sessions.length;
  const total = n + 1;
  const cols = total <= 1 ? 1 : total <= 4 ? 2 : total <= 9 ? 3 : 4;
  return (
    <div className="grid-view">
      <div className="grid-head muted small">{t("grid.summary", { n })}</div>
      <div className="grid" style={{ gridTemplateColumns: `repeat(${cols}, minmax(0, 1fr))` }}>
        {sessions.map((s) => (
          <Tile key={s.id} s={s} />
        ))}
        <button className="tile add" onClick={store.newTab}>
          <IconPlus size={26} />
          <span>{t("grid.add")}</span>
        </button>
      </div>
    </div>
  );
}

function Tile({ s }: { s: RemoteSession }) {
  const t = useT();
  useSession(s);
  const name = useSessionTitle(s);
  return (
    <div className={`tile ${s.state}`} onClick={() => store.setView(s.id)} title={t("grid.open")}>
      <div className="tile-screen">
        <CanvasHost s={s} interactive={false} />
        {s.state !== "live" && (
          <div className="tile-overlay">
            {s.state === "closed" || s.state === "waiting" ? (
              <span>{sessionStateText(s)}</span>
            ) : (
              <span className="spinner" />
            )}
          </div>
        )}
      </div>
      <div className="tile-bar">
        <span className={`dot ${s.state}`} />
        <span className="tile-name">{name}</span>
        <span className="muted small tile-stats">{s.state === "live" ? sessionStateText(s) : fmtCode(s.code)}</span>
        <button
          className="icon-btn subtle"
          title={t("common.disconnect")}
          onClick={(e) => {
            e.stopPropagation();
            closeTab(s.id);
          }}
        >
          <IconClose size={14} />
        </button>
      </div>
    </div>
  );
}
