# nginx image for the zellij-relay testbed.
#
# The hostname is baked in at build time because Let's Encrypt cert paths
# must be literal. Rebuild the image any time the VPS IP changes.

FROM nginx:1.27-alpine

ARG PUBLIC_HOST
RUN test -n "${PUBLIC_HOST}" || (echo "PUBLIC_HOST build arg is required" && exit 1)

RUN apk add --no-cache gettext

COPY nginx.conf.template /etc/nginx/nginx.conf.template
COPY proxy-ws.conf       /etc/nginx/snippets/proxy-ws.conf

RUN envsubst '${PUBLIC_HOST}' < /etc/nginx/nginx.conf.template \
    > /etc/nginx/nginx.conf \
 && rm /etc/nginx/nginx.conf.template \
 && nginx -t -c /etc/nginx/nginx.conf || \
    (echo "NOTE: initial nginx -t fails before the cert volume is present — this is expected at build time." >&2 && true)

EXPOSE 80 443
