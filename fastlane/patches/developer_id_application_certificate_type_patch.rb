require "cert/runner"
require "spaceship"

module VizorFastlanePatches
  module DeveloperIdApplicationCertificateTypePatch
    def certificate_type
      if Cert.config[:type].to_s == "developer_id_application"
        return Spaceship::ConnectAPI::Certificate::CertificateType::DEVELOPER_ID_APPLICATION
      end

      super
    end
  end
end

Cert::Runner.prepend(VizorFastlanePatches::DeveloperIdApplicationCertificateTypePatch)
