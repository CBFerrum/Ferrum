// Operations' own screens as the kit's tabs: the same tab strip, keys and look as every record workspace.

import type { ReactNode } from "react";
import { useNavigate } from "react-router";
import { t } from "@ferrum/client";
import { PageHeader, TabPanel, Tabs } from "@ferrum/kit";

export type Section = "sites" | "attention" | "commands";

const PATHS: Record<Section, string> = { sites: "/", attention: "/attention", commands: "/commands" };

/** A screen's frame: the kit's page head, the section tabs (with how many need a person), then the section. */
export function Screen({ section, attention, actions, children }: { section: Section; attention?: number; actions?: ReactNode; children: ReactNode }) {
  const go = useNavigate();
  const tabs: { id: Section; label: string }[] = [
    { id: "sites", label: t("Sites") },
    { id: "attention", label: `${t("Needs a person")}${attention ? ` (${attention})` : ""}` },
    { id: "commands", label: t("Commands") },
  ];
  return (
    <div>
      <PageHeader title={t("Operations")} actions={actions} />
      <Tabs id="ops" tabs={tabs} value={section} onChange={(id) => go(PATHS[id as Section])} />
      <TabPanel id="ops" tab={section}>
        <div className="ops-page">{children}</div>
      </TabPanel>
    </div>
  );
}
