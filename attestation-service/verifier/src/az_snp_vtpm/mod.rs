// Copyright (c) Microsoft Corporation.
//
// SPDX-License-Identifier: Apache-2.0
//

use crate::{InitDataHash, ReportData};

use super::{TeeEvidenceParsedClaim, Verifier};
use crate::snp::{
    load_milan_cert_chain, parse_tee_evidence, verify_report_signature, VendorCertificates,
};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use az_snp_vtpm::certs::Vcek;
use az_snp_vtpm::hcl::HclReport;
use az_snp_vtpm::report::AttestationReport;
use az_snp_vtpm::vtpm::Quote;
use log::{debug, warn};
use openssl::pkey::PKey;
use serde::{Deserialize, Serialize};
use sev::firmware::host::{CertTableEntry, CertType};

const HCL_VMPL_VALUE: u32 = 0;

#[derive(Serialize, Deserialize)]
struct Evidence {
    quote: Quote,
    report: Vec<u8>,
    vcek: String,
}

pub struct AzSnpVtpm {
    vendor_certs: VendorCertificates,
}

impl AzSnpVtpm {
    pub fn new() -> Result<Self> {
        let Result::Ok(vendor_certs) = load_milan_cert_chain() else {
            bail!("Failed to load Milan cert chain");
        };
        let vendor_certs = vendor_certs.clone();
        Ok(Self { vendor_certs })
    }
}

#[async_trait]
impl Verifier for AzSnpVtpm {
    /// The following verification steps are performed:
    /// 1. TPM Quote has been signed by AK included in the HCL variable data
    /// 2. Attestation report_data matches TPM Quote nonce
    /// 3. TPM PCRs' digest matches the digest in the Quote
    /// 4. SNP report's report_data field matches hashed HCL variable data
    /// 5. SNP Report is genuine
    /// 6. SNP Report has been issued in VMPL 0
    async fn evaluate(
        &self,
        evidence: &[u8],
        expected_report_data: &ReportData,
        expected_init_data_hash: &InitDataHash,
    ) -> Result<TeeEvidenceParsedClaim> {
        let ReportData::Value(expected_report_data) = expected_report_data else {
            bail!("unexpected empty report data");
        };

        if let InitDataHash::Value(_) = expected_init_data_hash {
            warn!("Azure SNP vTPM verifier does not support verify init data hash, will ignore the input `init_data_hash`.");
        }

        let evidence = serde_json::from_slice::<Evidence>(evidence)
            .context("Failed to deserialize Azure vTPM SEV-SNP evidence")?;

        let hcl_report = HclReport::new(evidence.report)?;
        verify_signature(&evidence.quote, &hcl_report)?;

        verify_nonce(&evidence.quote, expected_report_data)?;

        verify_pcrs(&evidence.quote)?;

        let var_data_hash = hcl_report.var_data_sha256();
        let snp_report = hcl_report.try_into()?;
        verify_report_data(&var_data_hash, &snp_report)?;

        let vcek = Vcek::from_pem(&evidence.vcek)?;
        verify_snp_report(&snp_report, &vcek, &self.vendor_certs)?;

        let claim = parse_tee_evidence(&snp_report);
        Ok(claim)
    }
}

fn verify_nonce(quote: &Quote, report_data: &[u8]) -> Result<()> {
    let nonce = quote.nonce()?;
    if nonce != report_data[..] {
        bail!("TPM quote nonce doesn't match expected report_data");
    }
    debug!("TPM report_data verification completed successfully");
    Ok(())
}

fn verify_signature(quote: &Quote, hcl_report: &HclReport) -> Result<()> {
    let ak_pub = hcl_report.ak_pub().context("Failed to get AKpub")?;
    let der = ak_pub.key.try_to_der()?;
    let ak_pub = PKey::public_key_from_der(&der).context("Failed to parse AKpub")?;

    quote
        .verify_signature(&ak_pub)
        .context("vTPM quote is not signed by AKpub")?;
    debug!("Signature verification completed successfully");
    Ok(())
}

fn verify_pcrs(quote: &Quote) -> Result<()> {
    quote
        .verify_pcrs()
        .context("Digest of PCRs does not match digest in Quote")?;
    debug!("PCR verification completed successfully");
    Ok(())
}

fn verify_report_data(var_data_hash: &[u8; 32], snp_report: &AttestationReport) -> Result<()> {
    if *var_data_hash != snp_report.report_data[..32] {
        bail!("SNP report report_data mismatch");
    }
    debug!("SNP report_data verification completed successfully");
    Ok(())
}

fn verify_snp_report(
    snp_report: &AttestationReport,
    vcek: &Vcek,
    vendor_certs: &VendorCertificates,
) -> Result<()> {
    let vcek_data = vcek.0.to_der().context("Failed to get raw VCEK data")?;
    let cert_chain = [CertTableEntry::new(CertType::VCEK, vcek_data)];
    verify_report_signature(snp_report, &cert_chain, vendor_certs)?;

    if snp_report.vmpl != HCL_VMPL_VALUE {
        bail!("VMPL of SNP report is not {HCL_VMPL_VALUE}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use az_snp_vtpm::vtpm::VerifyError;

    const REPORT: &[u8; 2600] = include_bytes!("../../test_data/az-snp-vtpm/hcl-report.bin");
    const QUOTE: &[u8; 1362] = include_bytes!("../../test_data/az-snp-vtpm/quote.bin");
    const REPORT_DATA: &[u8] = "challenge".as_bytes();

    #[test]
    fn test_verify_snp_report() {
        let hcl_report = HclReport::new(REPORT.to_vec()).unwrap();
        let snp_report = hcl_report.try_into().unwrap();
        let vcek = Vcek::from_pem(include_str!("../../test_data/az-snp-vtpm/vcek.pem")).unwrap();
        let vendor_certs = load_milan_cert_chain().as_ref().unwrap();
        verify_snp_report(&snp_report, &vcek, vendor_certs).unwrap();
    }

    #[test]
    fn test_verify_snp_report_failure() {
        let mut wrong_report = REPORT.clone();
        // messing with snp report
        wrong_report[0x01a6] = 0;
        let hcl_report = HclReport::new(wrong_report.to_vec()).unwrap();
        let snp_report = hcl_report.try_into().unwrap();
        let vcek = Vcek::from_pem(include_str!("../../test_data/az-snp-vtpm/vcek.pem")).unwrap();
        let vendor_certs = load_milan_cert_chain().as_ref().unwrap();
        assert_eq!(
            verify_snp_report(&snp_report, &vcek, vendor_certs)
                .unwrap_err()
                .to_string(),
            "SNP version mismatch",
        );
    }

    #[test]
    fn test_verify_report_data() {
        let hcl_report = HclReport::new(REPORT.to_vec()).unwrap();
        let var_data_hash = hcl_report.var_data_sha256();
        let snp_report = hcl_report.try_into().unwrap();
        verify_report_data(&var_data_hash, &snp_report).unwrap();
    }

    #[test]
    fn test_verify_report_data_failure() {
        let mut wrong_report = REPORT.clone();
        wrong_report[0x06e0] += 1;
        let hcl_report = HclReport::new(wrong_report.to_vec()).unwrap();
        let var_data_hash = hcl_report.var_data_sha256();
        let snp_report = hcl_report.try_into().unwrap();
        assert_eq!(
            verify_report_data(&var_data_hash, &snp_report)
                .unwrap_err()
                .to_string(),
            "SNP report report_data mismatch"
        );
    }

    #[test]
    fn test_verify_signature() {
        let quote: Quote = bincode::deserialize(QUOTE).unwrap();
        let hcl_report = HclReport::new(REPORT.to_vec()).unwrap();
        verify_signature(&quote, &hcl_report).unwrap();
    }

    #[test]
    fn test_verify_quote_signature_failure() {
        let mut quote = QUOTE.clone();
        quote[0x030] = 0;
        let wrong_quote: Quote = bincode::deserialize(&quote).unwrap();

        let hcl_report = HclReport::new(REPORT.to_vec()).unwrap();
        assert_eq!(
            verify_signature(&wrong_quote, &hcl_report)
                .unwrap_err()
                .downcast_ref::<VerifyError>()
                .unwrap()
                .to_string(),
            VerifyError::SignatureMismatch.to_string()
        );
    }

    #[test]
    fn test_verify_akpub_failure() {
        let quote: Quote = bincode::deserialize(QUOTE).unwrap();
        let mut wrong_report = REPORT.clone();
        // messing with AKpub in var data
        wrong_report[0x0540] = 0;
        let wrong_hcl_report = HclReport::new(wrong_report.to_vec()).unwrap();
        assert_eq!(
            verify_signature(&quote, &wrong_hcl_report)
                .unwrap_err()
                .to_string(),
            "Failed to get AKpub",
        );
    }

    #[test]
    fn test_verify_quote_nonce() {
        let quote: Quote = bincode::deserialize(QUOTE).unwrap();
        verify_nonce(&quote, &REPORT_DATA).unwrap();
    }

    #[test]
    fn test_verify_quote_nonce_failure() {
        let quote: Quote = bincode::deserialize(QUOTE).unwrap();
        let mut wrong_report_data = REPORT_DATA.to_vec();
        wrong_report_data.reverse();
        verify_nonce(&quote, &wrong_report_data).unwrap_err();
    }

    #[test]
    fn test_verify_pcrs() {
        let quote: Quote = bincode::deserialize(QUOTE).unwrap();
        verify_pcrs(&quote).unwrap();
    }

    #[test]
    fn test_verify_pcrs_failure() {
        let mut quote = QUOTE.clone();
        quote[0x0169] = 0;
        let wrong_quote: Quote = bincode::deserialize(&quote).unwrap();

        assert_eq!(
            verify_pcrs(&wrong_quote)
                .unwrap_err()
                .downcast_ref::<VerifyError>()
                .unwrap()
                .to_string(),
            VerifyError::PcrMismatch.to_string()
        );
    }
}
