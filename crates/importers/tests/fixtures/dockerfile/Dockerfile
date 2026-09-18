# syntax=docker/dockerfile:1
FROM node:20 AS build
WORKDIR /app
COPY . .
RUN npm ci && \
    npm run build

FROM node:20-slim AS runtime
ENV PORT=8080
WORKDIR /app
COPY --from=build /app/dist ./dist
EXPOSE $PORT
HEALTHCHECK --interval=10s --timeout=3s \
  CMD curl -fsS http://localhost:8080/healthz || exit 1
ENTRYPOINT ["tini", "--"]
CMD ["node", "dist/server.js"]
