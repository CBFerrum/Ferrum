// How devices are described. A definition is written as text and tried on a sample here, with no
// device attached, by the same code a gateway runs. Saving reaches nothing: publishing does.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useId, useMemo, useState } from "react";
import { Link, useNavigate, useParams } from "react-router";
import { refusal } from "@ferrum/client";
import { Alert, Badge, Button, Dialog, EmptyState, ErrorState, Field, PageLoading, toast, useShellTitle, useUnsavedGuard } from "@ferrum/kit";
import { definitionDoc, definitionList, newDefinition, publishDefinition, saveDefinition, type Template, templates, tryDefinition, type Tried } from "./data";
import { Tabs } from "./Tabs";

function Start({ onClose }: { onClose: () => void }) {
  const id = useId();
  const navigate = useNavigate();
  const list = useQuery({ queryKey: ["edge", "templates"], queryFn: templates, staleTime: Infinity });
  const [choice, setChoice] = useState<Template | null>(null);
  const [name, setName] = useState("");
  const make = useMutation({
    mutationFn: () => newDefinition(name.trim(), choice?.body ?? "name: new-device\nconnect: tcp://device:9100\n"),
    onSuccess: (made) => navigate(`/definitions/${made.id}`),
    onError: (e) => toast(refusal(e), "error"),
  });
  const groups = useMemo(() => {
    const by = new Map<string, Template[]>();
    for (const t of list.data?.templates ?? []) by.set(t.group, [...(by.get(t.group) ?? []), t]);
    return [...by.entries()];
  }, [list.data]);
  return (
    <Dialog
      open
      title="A new definition"
      onClose={onClose}
      footer={
        <>
          <Button onClick={onClose}>Back</Button>
          <Button variant="primary" disabled={!name.trim()} loading={make.isPending} onClick={() => make.mutate()}>
            Start from {choice ? "this" : "nothing"}
          </Button>
        </>
      }
    >
      <div className="stack">
        <Field label="Name" htmlFor={`${id}-n`} required>
          <input id={`${id}-n`} className="input" value={name} onChange={(e) => setName(e.target.value)} />
        </Field>
        {list.isPending ? (
          <PageLoading />
        ) : list.isError ? (
          <ErrorState error={list.error} />
        ) : (
          <div className="edge-templates">
            {groups.map(([group, items]) => (
              <fieldset key={group}>
                <legend>{group}</legend>
                {items.map((t) => (
                  <label key={t.name} className={choice === t ? "edge-template edge-on" : "edge-template"}>
                    <input
                      type="radio"
                      name={`${id}-t`}
                      checked={choice === t}
                      onChange={() => {
                        setChoice(t);
                        setName((n) => n || t.name);
                      }}
                    />
                    <span>
                      <strong>{t.name}</strong>
                      <span className="faint">{t.about}</span>
                    </span>
                  </label>
                ))}
              </fieldset>
            ))}
            <p className="faint">A template is a start, not a promise: none has been tried on the equipment it names.</p>
          </div>
        )}
      </div>
    </Dialog>
  );
}

export function Definitions() {
  useShellTitle("Definitions");
  const q = useQuery({ queryKey: ["edge", "definitions"], queryFn: definitionList });
  const [starting, setStarting] = useState(false);
  return (
    <div className="edge-page">
      <Tabs />
      <div className="edge-bar">
        <Button variant="primary" onClick={() => setStarting(true)}>
          New definition
        </Button>
      </div>
      {q.isPending ? (
        <PageLoading />
      ) : q.isError ? (
        <ErrorState error={q.error} onRetry={() => void q.refetch()} />
      ) : q.data.rows.length === 0 ? (
        <EmptyState title="No definitions yet" icon="doc">
          <p className="muted">A definition says how a kind of device is reached, read and commanded, in a few lines.</p>
        </EmptyState>
      ) : (
        <ul className="card edge-rows edge-rows-pad">
          {q.data.rows.map((d) => (
            <li key={d.id}>
              <Link to={`/definitions/${d.id}`} className="edge-row-main">
                <strong>{d.definition_name}</strong>
                <div className="faint">{d.revision ? `Version ${d.revision}` : "Never published"}</div>
              </Link>
              <Badge tone={d.status === "Published" ? "green" : "neutral"}>{d.status}</Badge>
            </li>
          ))}
        </ul>
      )}
      {starting ? <Start onClose={() => setStarting(false)} /> : null}
    </div>
  );
}

function Result({ tried }: { tried: Tried }) {
  if (!tried.ok) return <Alert tone="error">{tried.error}</Alert>;
  if (!tried.decoded?.length) return <Alert tone="info">It is well formed, and read nothing from that sample.</Alert>;
  return (
    <ul className="edge-rows">
      {tried.decoded.map((d, i) => (
        <li key={i}>
          <div className="edge-row-main">
            <strong>{d.name ?? d.kind}</strong>
            <div className="faint">
              {d.kind}
              {d.device ? `, for ${d.device}` : ""}
            </div>
          </div>
          <span>
            {typeof d.value === "string" ? d.value : JSON.stringify(d.value)}
            {d.unit ? ` ${d.unit}` : ""}
          </span>
        </li>
      ))}
    </ul>
  );
}

export function DefinitionPage() {
  const { id } = useParams() as { id: string };
  const form = useId();
  const qc = useQueryClient();
  const q = useQuery({ queryKey: ["edge", "definition", id], queryFn: () => definitionDoc(id) });
  useShellTitle(q.data?.definition_name ?? "Definition");
  const [body, setBody] = useState("");
  const [topic, setTopic] = useState("");
  const [sample, setSample] = useState("");
  const [hex, setHex] = useState(false);
  const [tried, setTried] = useState<Tried | null>(null);
  useEffect(() => {
    if (q.data) setBody(q.data.body);
  }, [q.data]);
  const dirty = q.data !== undefined && body !== q.data.body;
  const { dialog } = useUnsavedGuard(dirty);
  const attempt = useMutation({ mutationFn: () => tryDefinition(body, topic, sample, hex), onSuccess: setTried, onError: (e) => toast(refusal(e), "error") });
  const save = useMutation({
    mutationFn: () => saveDefinition(id, q.data!.version, { body }),
    onSuccess: (doc) => {
      qc.setQueryData(["edge", "definition", id], doc);
      toast("Saved. Gateways are still running what was last published.");
    },
    onError: (e) => toast(refusal(e), "error"),
  });
  const publish = useMutation({
    mutationFn: async () => {
      if (dirty) await save.mutateAsync();
      return publishDefinition(id);
    },
    onSuccess: () => {
      toast("Published. Gateways take it at their next exchange.");
      void qc.invalidateQueries({ queryKey: ["edge"] });
    },
    onError: (e) => toast(refusal(e), "error"),
  });
  if (q.isPending) return <PageLoading />;
  if (q.isError) return <ErrorState error={q.error} onRetry={() => void q.refetch()} />;
  return (
    <div className="edge-page">
      <Tabs />
      <div className="edge-bar">
        <Link to="/definitions">Definitions</Link>
        <Badge tone={q.data.status === "Published" ? "green" : "neutral"}>{q.data.revision ? `Version ${q.data.revision}` : "Never published"}</Badge>
        <span className="edge-grow" />
        <Button disabled={!dirty} loading={save.isPending} onClick={() => save.mutate()}>
          Save
        </Button>
        <Button variant="primary" loading={publish.isPending} onClick={() => publish.mutate()}>
          Publish
        </Button>
      </div>
      <div className="edge-split">
        <section className="card card-pad stack" aria-label="The definition">
          <Field label="Definition" htmlFor={`${form}-b`} hint="YAML. See the definitions guide for every key.">
            <textarea id={`${form}-b`} className="input edge-code" spellCheck={false} rows={24} value={body} onChange={(e) => setBody(e.target.value)} />
          </Field>
        </section>
        <section className="card card-pad stack" aria-label="Try it">
          <h2 className="edge-section">Try it on a sample</h2>
          <p className="muted">This runs here, by the code a gateway runs. Nothing reaches a device.</p>
          <Field label="Topic or path" htmlFor={`${form}-t`} hint="Leave empty for a stream.">
            <input id={`${form}-t`} className="input" value={topic} onChange={(e) => setTopic(e.target.value)} />
          </Field>
          <Field label="What arrives" htmlFor={`${form}-s`}>
            <textarea id={`${form}-s`} className="input edge-code" spellCheck={false} rows={6} value={sample} onChange={(e) => setSample(e.target.value)} />
          </Field>
          <label className="edge-check">
            <input type="checkbox" checked={hex} onChange={(e) => setHex(e.target.checked)} /> That is hexadecimal
          </label>
          <Button loading={attempt.isPending} onClick={() => attempt.mutate()}>
            Try it
          </Button>
          {tried ? <Result tried={tried} /> : null}
        </section>
      </div>
      {dialog}
    </div>
  );
}
