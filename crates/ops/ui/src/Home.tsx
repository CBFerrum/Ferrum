// Every site at a glance. What matters first: can we see it, is it working, and does anything need
// a person. A silent gateway may be working perfectly well offline, so the screen says what it does
// not know rather than guessing.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useId, useState } from "react";
import { Link } from "react-router";
import { refusal, relativeTime, t, isolate } from "@ferrum/client";
import { LayoutDashboardIcon } from "lucide-react";
import { Alert, Badge, Button, Dialog, Field, LoadFailed, PageLoading, StatePanel, StatusBadge, toast } from "@ferrum/kit";
import { LinkInput } from "@ferrum/records";
import { type Domain, overview, setPin, type SiteRow, transfer } from "./data";
import { Screen } from "./Tabs";
import { LIFE_TONES, silence } from "./words";

const megabytes = (bytes?: number) => (bytes === undefined ? null : t("{megabytes} MB free", { megabytes: Math.round(bytes / (1 << 20)) }));

function Transfer({ domain, onClose }: { domain: Domain; onClose: () => void }) {
  const id = useId();
  const qc = useQueryClient();
  const [to, setTo] = useState<string | null>(null);
  const [reason, setReason] = useState("");
  const move = useMutation({
    mutationFn: () => transfer(domain.domain, to!, reason),
    onSuccess: () => {
      toast(t("Moved. The old gateway has been told to stop."));
      void qc.invalidateQueries({ queryKey: ["ops"] });
      onClose();
    },
    onError: (e) => toast(refusal(e), "error"),
  });
  return (
    <Dialog
      open
      title={t("Move this site to another gateway")}
      onClose={onClose}
      footer={
        <>
          <Button onClick={onClose}>{t("Keep it as it is")}</Button>
          <Button variant="primary" disabled={!to || !reason.trim()} loading={move.isPending} onClick={() => move.mutate()}>
            {t("Move it")}
          </Button>
        </>
      }
    >
      <div className="stack">
        <Alert tone="warning">{t("The old gateway stops being believed at once. Anything it may have started becomes “Nobody knows”, and someone has to look at the equipment. Do this only when the old gateway is really gone.")}</Alert>
        <Field label={t("New gateway")} htmlFor={`${id}-to`} required>
          <LinkInput to="Device" id={`${id}-to`} value={to} required linkFilters={{ kind: "Gateway" }} onChange={setTo} />
        </Field>
        <Field label={t("Why")} htmlFor={`${id}-why`} required>
          <input id={`${id}-why`} className="input" value={reason} onChange={(e) => setReason(e.target.value)} />
        </Field>
      </div>
    </Dialog>
  );
}

/** The site's console works with the link down, so it cannot ask the platform who someone is. */
function Pin({ site, onClose }: { site: SiteRow; onClose: () => void }) {
  const id = useId();
  const [email, setEmail] = useState("");
  const [pin, setValue] = useState("");
  const save = useMutation({
    mutationFn: () => setPin(site.id, email.trim(), pin),
    onSuccess: () => {
      toast(t("Saved. It reaches the site at its next exchange."));
      onClose();
    },
    onError: (e) => toast(refusal(e), "error"),
  });
  return (
    <Dialog
      open
      title={t("Console PIN at {name}", { name: site.name })}
      onClose={onClose}
      footer={
        <>
          <Button onClick={onClose}>{t("Back")}</Button>
          <Button variant="primary" disabled={!email.trim() || pin.length < 6} loading={save.isPending} onClick={() => save.mutate()}>
            {t("Save it")}
          </Button>
        </>
      }
    >
      <div className="stack">
        <Field label={t("Email of the person")} htmlFor={`${id}-e`} required>
          <input id={`${id}-e`} className="input" type="email" autoComplete="off" value={email} onChange={(e) => setEmail(e.target.value)} />
        </Field>
        <Field label={t("PIN")} htmlFor={`${id}-p`} required hint={t("At least six digits. Replaces the one they had.")}>
          <input id={`${id}-p`} className="input" type="password" inputMode="numeric" autoComplete="new-password" value={pin} onChange={(e) => setValue(e.target.value.replace(/\D/g, ""))} />
        </Field>
      </div>
    </Dialog>
  );
}

function DomainRow({ d }: { d: Domain }) {
  const [moving, setMoving] = useState(false);
  const seen = silence(d.silent_secs);
  const h = d.health;
  return (
    <div className="ops-domain">
      <div className="ops-domain-head">
        <div>
          <strong>{d.holder_name ?? t("No gateway")}</strong> <Badge tone={seen.tone}>{d.holder ? seen.label : t("Nobody holds this")}</Badge>
          <div className="faint">{d.last_seen_at ? t("Last heard {value}", { value: relativeTime(d.last_seen_at) }) : t("Not heard from yet")}</div>
        </div>
        <Button onClick={() => setMoving(true)}>
          {t("Move…")}
        </Button>
      </div>
      {h?.stopped ? <Alert tone="error">{isolate(h.stopped)}</Alert> : null}
      <ul className="ops-facts">
        {h?.version ? <li>{t("Runtime {version}", { version: h.version })}</li> : null}
        {h?.disk_free_bytes ? <li>{isolate(megabytes(h.disk_free_bytes) ?? "")}</li> : null}
        {h?.backlog_secs && h.backlog_secs > 60 ? <li>{t("{minutes} min of reports still to arrive", { minutes: Math.round(h.backlog_secs / 60) })}</li> : null}
        {h?.telemetry_shed ? <li>{t("{telemetry_shed} measurements given up", { telemetry_shed: h.telemetry_shed })}</li> : null}
        {Object.entries(h?.adapters ?? {}).map(([name, state]) => (
          <li key={name}>
            {isolate(name)}: {isolate(state)}
          </li>
        ))}
      </ul>
      {moving ? <Transfer domain={d} onClose={() => setMoving(false)} /> : null}
    </div>
  );
}

function Site({ s }: { s: SiteRow }) {
  const [pin, setAsking] = useState(false);
  return (
    <section className="card card-pad ops-site" aria-label={s.name}>
      <header className="ops-site-head">
        <div>
          <h2>{isolate(s.name)}</h2>
          <div className="faint">{t("Can run alone for {max_offline_hours} h", { max_offline_hours: s.max_offline_hours })}</div>
        </div>
        {s.needs_person ? (
          <Link to="/attention" className="ops-need">
            {s.needs_person === 1 ? t("1 needs a person") : t("{n} need a person", { n: s.needs_person })}
          </Link>
        ) : (
          <Button onClick={() => setAsking(true)}>
            {t("Console PIN…")}
          </Button>
        )}
      </header>
      <dl className="ops-counts">
        <div>
          <dt>{t("Waiting")}</dt>
          <dd>{s.waiting}</dd>
        </div>
        <div>
          <dt>{t("Running")}</dt>
          <dd>{s.running}</dd>
        </div>
      </dl>
      {s.domains.length === 0 ? <p className="muted">{t("No gateway has enrolled here yet.")}</p> : s.domains.map((d) => <DomainRow key={d.domain} d={d} />)}
      {s.devices ? (
        <div className="ops-life">
          {Object.entries(s.devices).map(([state, n]) => (
            <StatusBadge key={state} value={state} tones={LIFE_TONES} label={<>{n} {isolate(state.toLowerCase())}</>} />
          ))}
        </div>
      ) : null}
      {pin ? <Pin site={s} onClose={() => setAsking(false)} /> : null}
    </section>
  );
}

export function Home() {
  const q = useQuery({ queryKey: ["ops", "overview"], queryFn: overview, refetchInterval: 15_000 });
  const sites = q.data?.sites ?? [];
  const attention = sites.reduce((n, s) => n + s.needs_person, 0);
  return (
    <Screen section="sites" attention={attention}>
      {q.isPending ? (
        <PageLoading />
      ) : q.isError ? (
        <LoadFailed error={q.error} onRetry={() => void q.refetch()} />
      ) : sites.length === 0 ? (
        <StatePanel kind="empty" icon={LayoutDashboardIcon} title={t("No sites yet")} action={<Link className="btn btn-primary btn-md" to="/e/Site/new">{t("Add your first site")}</Link>} />
      ) : (
        <div className="ops-sites">
          {sites.map((s) => (
            <Site key={s.id} s={s} />
          ))}
        </div>
      )}
    </Screen>
  );
}
