<?xml version='1.0' encoding='UTF-8'?>
<!--
  Maps `builtins.toXML` output (Nix' <expr><attrs><attr name=...><string|int/>)
  onto a libvirt LXC domain.  The Nix side only supplies data; every piece of
  the libvirt schema lives here.

    echo <toXML> | xsltproc libvirt-domain.xsl -
-->
<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
  <xsl:output method="xml" indent="yes" omit-xml-declaration="yes"/>
  <xsl:strip-space elements="*"/>

  <xsl:template match="/expr">
    <xsl:variable name="d" select="attrs"/>
    <domain type="lxc">
      <name><xsl:value-of select="$d/attr[@name='name']/*/@value"/></name>
      <memory unit="KiB"><xsl:value-of select="number($d/attr[@name='memory']/*/@value) * 1024"/></memory>
      <currentMemory unit="KiB"><xsl:value-of select="number($d/attr[@name='memory']/*/@value) * 1024"/></currentMemory>
      <vcpu placement="static"><xsl:value-of select="$d/attr[@name='vcpus']/*/@value"/></vcpu>
      <xsl:if test="$d/attr[@name='cpuQuota']">
        <cputune>
          <quota><xsl:value-of select="$d/attr[@name='cpuQuota']/*/@value"/></quota>
          <period><xsl:value-of select="$d/attr[@name='cpuPeriod']/*/@value"/></period>
        </cputune>
      </xsl:if>
      <xsl:if test="$d/attr[@name='swap']">
        <memtune>
          <swap_hard_limit unit="KiB"><xsl:value-of select="number($d/attr[@name='swap']/*/@value) * 1024"/></swap_hard_limit>
        </memtune>
      </xsl:if>
      <os>
        <type arch="x86_64">exe</type>
        <init><xsl:value-of select="$d/attr[@name='init']/*/@value"/></init>
      </os>
      <!-- User-namespace mapping: present exactly when the Nix side set `idmap`,
           which is what makes the domain unprivileged.  libvirt fixes `start`
           at 0 for both maps and takes target/count from the data tree. -->
      <xsl:if test="$d/attr[@name='idmap']">
        <idmap>
          <uid start="0" target="{$d/attr[@name='idmap']/attrs/attr[@name='target']/*/@value}" count="{$d/attr[@name='idmap']/attrs/attr[@name='count']/*/@value}"/>
          <gid start="0" target="{$d/attr[@name='idmap']/attrs/attr[@name='target']/*/@value}" count="{$d/attr[@name='idmap']/attrs/attr[@name='count']/*/@value}"/>
        </idmap>
      </xsl:if>
      <clock offset="utc"/>
      <on_poweroff>destroy</on_poweroff>
      <on_reboot>restart</on_reboot>
      <on_crash>restart</on_crash>
      <devices>
        <filesystem type="mount" accessmode="passthrough">
          <source dir="{$d/attr[@name='rootfs']/*/@value}"/>
          <target dir="/"/>
        </filesystem>
        <interface type="bridge">
          <mac address="{$d/attr[@name='mac']/*/@value}"/>
          <source bridge="{$d/attr[@name='bridge']/*/@value}"/>
          <guest dev="{$d/attr[@name='ifname']/*/@value}"/>
        </interface>
        <console type="pty">
          <target type="lxc" port="0"/>
        </console>
        <xsl:for-each select="$d/attr[@name='extraXML']/list/string/@value">
          <xsl:value-of select="." disable-output-escaping="yes"/>
        </xsl:for-each>
      </devices>
      <seclabel type="none"/>
    </domain>
  </xsl:template>
</xsl:stylesheet>
