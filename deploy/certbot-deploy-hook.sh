#!/bin/sh
# Certbot deploy hook: hand a new or renewed certificate to Caddy.
#
# Used with Let's Encrypt IP address certificates (see docs/deploy.md), which
# certbot obtains but can't install into Caddy itself. Certbot runs this after
# every successful issuance and renewal, with RENEWED_LINEAGE set to the
# certificate's directory (/etc/letsencrypt/live/<name>).
#
# Install: sudo install -m 755 certbot-deploy-hook.sh /usr/local/bin/mpvlink-caddy-cert-hook

set -eu

dest=/etc/caddy/certs

# certbot's own files are root-only; give Caddy (group "caddy") read access to a copy.
install -d -m 750 -o root -g caddy "$dest"
install -m 640 -o root -g caddy "$RENEWED_LINEAGE/fullchain.pem" "$dest/fullchain.pem.new"
install -m 640 -o root -g caddy "$RENEWED_LINEAGE/privkey.pem" "$dest/privkey.pem.new"
mv -f "$dest/fullchain.pem.new" "$dest/fullchain.pem"
mv -f "$dest/privkey.pem.new" "$dest/privkey.pem"

# Caddy doesn't watch certificate files; a reload makes it use the new ones.
# mpvlink clients reconnect and resume on their own, so video isn't interrupted.
if systemctl is-active --quiet caddy; then
    systemctl reload caddy
fi
