## X. Legal, Regulatory, and Intellectual Property Compliance

### X.1 Purpose
This policy establishes [Company Name]'s commitment to identifying, tracking,
and complying with all applicable legislative, regulatory, and contractual
requirements, including those related to intellectual property rights,
that affect our business processes and software products.

### X.2 Scope
This policy applies to all employees, contractors, consultants, and third
parties who access [Company Name] systems, data, or perform work on behalf
of the company. It covers all company-developed software, customer data,
third-party software and services, and business operations.

### X.3 Policy Ownership
- **Policy Owner:** [CTO / Head of Security / Founder]
- **Review Cadence:** At least annually, or upon material regulatory change.
- **Approval:** [CEO / Board / Security Committee]

### X.4 Legislative and Regulatory Compliance

StreamingFast identifies and complies with laws and regulations applicable
to its operations and its customers' data, including but not limited to:

- **Québec privacy law:** An Act respecting the protection of personal
  information in the private sector (the "Private Sector Act"), as amended
  by Law 25 (formerly Bill 64), including appointment of a Person in
  Charge of the Protection of Personal Information (Privacy Officer),
  privacy impact assessments (PIAs) for projects involving personal
  information, breach notification to the Commission d'accès à
  l'information (CAI), and data portability rights.
- **Canadian federal privacy law:** Personal Information Protection and
  Electronic Documents Act (PIPEDA) for interprovincial and international
  data flows.
- **Other Canadian provincial laws:** As applicable where customers or
  data subjects are located (e.g., Alberta PIPA, BC PIPA).
- **International privacy law:** GDPR (EU), UK GDPR, CCPA/CPRA
  (California), and other jurisdictional privacy laws applicable to
  customer data we process.
- **Charter of the French Language (Bill 96):** French-language
  obligations for contracts of adhesion, consumer-facing communications,
  and workplace documentation where applicable.
- **Civil Code of Québec:** Governs contractual relationships,
  including confidentiality (arts. 2088, 2089) and obligations of
  good faith.
- **Employment law:** Act respecting labour standards (LNT),
  Pay Equity Act, and related Québec/federal employment legislation.
- **Export controls and sanctions:** Canadian Special Economic Measures
  Act (SEMA), Export and Import Permits Act, and equivalent U.S. OFAC
  regimes when transacting with U.S. parties.

### X.4.1 Privacy Officer (Required under Law 25)
StreamingFast has appointed a Person in Charge of the Protection of
Personal Information (Privacy Officer) as required by s. 3.1 of the
Private Sector Act. Contact details are published at [URL].

**Privacy Officer:** [Name, Title]
**Contact:** privacy@streamingfast.io

### X.4.2 Privacy Impact Assessments
PIAs are conducted for any project involving:
- Acquisition, development, or overhaul of an information system
  involving personal information
- Cross-border transfers of personal information outside Québec
- New uses or disclosures of personal information

### X.4.3 Breach Notification
Confidentiality incidents involving personal information are:
1. Logged in the incident register (required under Law 25)
2. Assessed for risk of serious injury
3. Notified to the CAI and affected individuals where risk is serious,
   without delay

### X.5 Contractual Compliance

1. All customer contracts, Data Processing Agreements (DPAs), Master Service
   Agreements (MSAs), and vendor agreements are reviewed prior to signature
   by [the Policy Owner / legal counsel].
2. Security and privacy commitments made in contracts (e.g., uptime SLAs,
   breach notification timelines, subprocessor disclosures, audit rights)
   are tracked and communicated to responsible internal owners.
3. Standard Contractual Clauses (SCCs) or equivalent transfer mechanisms
   are used for cross-border data transfers where required.
4. A subprocessor list is maintained and published at [URL] and updated
   when subprocessors change.

### X.6 Intellectual Property Rights

#### X.6.1 Company IP
1. All employees and contractors sign a Proprietary Information and
   Inventions Assignment Agreement (PIIAA) or equivalent at onboarding,
   assigning work product to [Company Name].
2. Confidentiality obligations survive termination of employment or
   engagement.
3. Trademarks, domains, and registered IP are maintained by [Policy Owner].

#### X.6.2 Third-Party and Open-Source Software
1. Use of third-party software must comply with the terms of its license.
2. A software bill of materials (SBOM) / dependency inventory is maintained
   for all production services.
3. Open-source dependencies are scanned automatically in CI using
   [Dependabot / Snyk / FOSSA / Trivy] for:
   - Known vulnerabilities (CVEs)
   - License compatibility
4. Licenses classified as incompatible with our commercial distribution
   (e.g., strong copyleft such as AGPL, SSPL) are prohibited in production
   code unless explicitly approved in writing by the Policy Owner.
5. Attribution requirements (e.g., MIT, BSD, Apache 2.0 NOTICE files) are
   honored in product documentation or an open-source acknowledgments page.

#### X.6.3 Customer Data and IP
1. Customer data remains the property of the customer, as set forth in
   our Terms of Service and DPAs.
2. [Company Name] does not use customer data to train models or for any
   purpose outside of delivering the contracted service, except as
   explicitly permitted by the customer.

### X.7 Records Retention
1. Contracts, signed policies, training records, and compliance evidence
   are retained for a minimum of [7] years or as required by applicable law.
2. Records are stored in [system — e.g., Google Drive with access controls,
   Ironclad, DocuSign, etc.] with access restricted to authorized personnel.

### X.8 Enforcement
Violations of this policy may result in disciplinary action up to and
including termination of employment or engagement, and may result in
civil or criminal liability.

### X.9 Exceptions
Exceptions to this policy require written approval from the Policy Owner
and are logged in the exceptions register.

### X.10 Related Documents
- Information Security Policy
- Acceptable Use Policy
- Data Classification and Handling Policy
- Vendor / Third-Party Management Policy
- Incident Response Plan
- Employee Handbook

### X.11 Revision History
| Version | Date       | Author   | Changes         |
|---------|------------|----------|-----------------|
| 1.0     | YYYY-MM-DD | [Name]   | Initial version |