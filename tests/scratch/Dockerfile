# The image holds the controller, a static busybox and the playbook, and nothing else: no libc,
# no shell, no /etc/passwd. The recipe `scratch-test` builds it from a context it assembles.
FROM scratch
COPY volant busybox inventory.ini site.yml /
WORKDIR /tmp
ENTRYPOINT ["/volant"]
