// Settings (server.md "UI"): `/settings/{general,integrations,model,members,audit,usage}`.
import { useNavigate, useParams } from "react-router";
import { Tabs } from "../components/common";
import { useMage } from "../context";
import { AuditSettings } from "./settings/Audit";
import { GeneralSettings } from "./settings/General";
import { IntegrationsSettings } from "./settings/Integrations";
import { MembersSettings } from "./settings/Members";
import { ModelSettings } from "./settings/Model";
import { UsageSettings } from "./settings/Usage";

export type SettingsSection = "general" | "integrations" | "model" | "members" | "audit" | "usage";

const SECTIONS: ReadonlyArray<{ id: SettingsSection; label: string }> = [
  { id: "general", label: "General" },
  { id: "integrations", label: "Integrations" },
  { id: "model", label: "Model" },
  { id: "members", label: "Members" },
  { id: "audit", label: "Audit" },
  { id: "usage", label: "Usage" },
];

export function SettingsPage() {
  const { link } = useMage();
  const navigate = useNavigate();
  const raw = useParams().section;
  const section = SECTIONS.find((s) => s.id === raw)?.id;
  if (section === undefined) return <p className="muted">No such settings page.</p>;
  return (
    <div className="mage-page">
      <header className="mage-head">
        <h1>Settings</h1>
      </header>
      <section className="card tabs-card">
        <Tabs label="Settings" tabs={SECTIONS} selected={section} onSelect={(s) => navigate(link(`settings/${s}`))}>
          {section === "general" && <GeneralSettings />}
          {section === "integrations" && <IntegrationsSettings />}
          {section === "model" && <ModelSettings />}
          {section === "members" && <MembersSettings />}
          {section === "audit" && <AuditSettings />}
          {section === "usage" && <UsageSettings />}
        </Tabs>
      </section>
    </div>
  );
}
