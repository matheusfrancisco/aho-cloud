import React from "react";
import { Trans, useTranslation } from "react-i18next";

export function Banner({ error }: { error?: string }) {
  const { t } = useTranslation();
  const onRetry = () => toast.error(t("errors.connection_not_found"));
  return (
    <Alert severity="error" onClose={onRetry}>
      <Trans i18nKey="errors.retry_later" />
      <Icons.Warning />
      {error ?? "Something went wrong"}
    </Alert>
  );
}
