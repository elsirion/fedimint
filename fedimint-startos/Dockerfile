FROM fedibtc/fedi-fedimintd:v0.10.0-fedi4-deployment1

# yq must be placed in the build context before building (see CI workflow)
COPY ./yq /usr/local/bin/yq

ADD ./docker_entrypoint.sh /docker_entrypoint.sh
ADD ./check-web.sh /usr/local/bin/check-web.sh
RUN chmod +x /docker_entrypoint.sh /usr/local/bin/check-web.sh

VOLUME ["/fedimintd", "/start-os"]
EXPOSE 8175/tcp

ENTRYPOINT ["/docker_entrypoint.sh"]
